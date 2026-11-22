# Changelog

All notable changes to `tickertape` and `tickertape-cli` are listed here. The format follows
[Keep a Changelog](https://keepachangelog.com/en/1.1.0/), and the crates follow [Semantic Versioning](https://semver.org/).

## [Unreleased]

- Fixed: with background checkpoints and size-based retention, retention removed only one segment per completed
  checkpoint whenever checkpoints took longer than the interval between requests (a writer that outruns the disk), so
  the journal grew far past its limit until the disk was full (issue #45; the long 1,024 B runs at `3fea1c0`). A
  retention run on the checkpoint thread now gives way to a queued checkpoint only after it has handled as many
  segments as the checkpoint it follows covered, or has taken as long as that checkpoint took, so it removes as many
  segments as the writer fills unless removals are slower than the checkpoints; it still stops after the removal in
  progress when `Writer::checkpoint`, `Writer::close` or dropping the writer waits for the thread. A queued background
  checkpoint can therefore wait for that many removals, and up to about as long again as the checkpoint before it,
  instead of one removal. ADR 0044 is amended; crash contract C14, ADR 0038, `docs/operations.md` and the `Writer::request_checkpoint` documentation
  say the same. Nothing changes in the C ABI or the bindings, which run the same code.
- Added (`tt-sustained`): the writer's per-second series and result record the checkpoints completed, the marker's
  position, the segments retention removed, its failures and last error, also as they stood before the final blocking
  checkpoint (`at_run_end`); a writer that stops with an error writes them to `writer-failure.json`, and the command
  prints them.
- Changed: retention no longer holds up checkpoints and `close` behind a writer that outruns the disk, and deleting a
  segment no longer reads it in full (issue #45, ADR 0044). The C ABI and the bindings run the same code, so the same
  applies to them; nothing is added to them. Not additive; every change a caller can see:
  - A deletion (`Action::Delete`, the writer's retention, `tickertape archive --delete`) checks the segment's header,
    its successor's header and position, the checkpoint, and that the segment's last non-zero eight bytes, read back
    from the end of the file, are a valid seal naming the successor; it no longer reads the records it discards. A
    sealed segment with a damaged record is therefore deleted, where it was refused with `Error::Corrupt`; so is one
    whose seal was lost if its last record ends, at an 8-byte boundary, with exactly the bytes of such a seal. When
    the end is not such a seal, the whole segment is scanned as before and the error is unchanged.
  - A move (`Action::MoveTo`, `tickertape archive --to`) still reads and validates the whole segment, now once instead
    of twice (in `plan` and again in `apply`).
  - `retention::plan` (and `tickertape archive --dry-run`) checks candidates as a deletion does: a sealed candidate with
    a damaged record is listed instead of failing the plan, and a move then stops at it with `Error::Corrupt` after
    moving the segments before it, where nothing was moved before.
  - With background checkpoints, the writer's retention run on the checkpoint thread stops before its next removal
    (after at least one planned segment) when another checkpoint is requested or `Writer::checkpoint`,
    `Writer::close` or dropping the writer waits for the thread; the run after the next checkpoint continues. These
    calls therefore wait for the removal in progress instead of the whole run, and segments that were eligible can
    remain after `close` until a later retention run (for example the next writer's at open). Retention at open and
    after a blocking `Writer::checkpoint` still runs to the end.
  - Added (`fault-injection` feature, not covered by the compatibility promise): `fault::Point::RetentionRemove` and
    `fault::Point::CheckpointWait`.
  - Docs: crash contract C14 states what is checked before a deletion and before a move; ADRs 0016 and 0038 are
    amended; `docs/benchmarks.md` (operating envelope), `docs/operations.md`, `docs/cli.md`, `docs/roadmap.md` and
    `docs/compatibility.md` say what changed and that the 1,024 B sustained scenarios are still to be measured.
- Fixed: a reader could report a record checksum mismatch (`Error::Corrupt`) for a record the writer had published
  correctly (issue #43). On aarch64 Linux (Google Cloud Axion VMs, kernel 7.0, ext4 and xfs) under memory pressure,
  readers that map a segment, including one written without tickertape code, saw a record whose commit word was
  visible while the bytes in its next page still read as zeros, with the file holding the record intact; the view
  caught up, but only after the three back-to-back re-reads a reader made before reporting corruption. A reader now
  reads a slot that fails validation again for at least 50 ms, with growing pauses, and reports corruption only if
  every re-read fails; it returns the record as soon as one validates. Real corruption is reported up to 50 ms later
  than before, also by `Reader::try_next`. The mechanism is not identified (`crash-contract.md` → Known risks and
  limits). The zero-copy reader reported it at `279f59a`; at `ddcbe49` the independent reader saw the same lag, and
  the zero-copy reader re-reads the same way there. **Known issue of v0.4.0** (and of every earlier release): their
  readers re-read the same way, so a reader's view lagging like this makes them report the same false corruption; it
  has not been tried with their writers, which have no background checkpoints.
- Added (bench tooling): when a `tt-sustained` reader reports corruption, it names the reader and its kind, prints
  what the file holds at that slot (commit word, stored and recomputed CRC, header fields, and the payload bytes that
  differ from what the writer sent, with their pages) right away and after 1 ms, 10 ms, 100 ms and 1 s, and reads the
  slot again through its own view at the same times, so that a wrong file can be told from a reader that saw the
  bytes wrong. The journal is kept as before.
- Docs: `docs/benchmarks.md` records the cloud results of 2026-11-19 to 2026-11-22 (Google Cloud VMs with local
  NVMe SSD, x86-64 and aarch64, ext4 and xfs): the sustained session with background checkpoints (64 B; its 1,024 B
  scenarios did not run because their probe failed), the reference session with blocking checkpoints before ADR 0038,
  the checkpoint measurements of ADR 0038, the sustained comparison with Chronicle Queue, Aeron, yaque, queue-file and
  plain files (tickertape's rows from a second session at a newer commit, since the first session's failed on the
  open-files defect fixed in #38), the readers' per-record cost, and replay and recovery at scale (the 10,000- and
  20,000-segment journals failed on the same defect). A new section states the operating envelope and the remaining
  bottlenecks. Open and not explained: a reader of the 1,024 B probe stopped with a record checksum mismatch on both
  aarch64 VMs at `279f59a`, and the same probe timed out on x86-64. ADRs 0025 (the ext4 stall at a roll: the roll's
  `fdatasync` waited for the ext4 journal commit; closed by #28), 0026, 0038 and 0042 point to the recorded results;
  the roadmap's status lines and the "Before 1.0" list of `docs/compatibility.md` are updated.
- Docs: `docs/benchmarks.md` records a second sustained session at `3fea1c0` (Google Cloud VMs with local NVMe SSD,
  x86-64 and aarch64, ext4 and xfs; tickertape with background checkpoints, both payloads): every run finished, and
  the 1,024 B scenarios ran for the first time (steady-50 and steady-90 keep their schedule on every VM); at 64 B and
  90 % of the probe the writer fell behind on aarch64 xfs. It also records the long verified runs (an unpaced 1,024 B
  writer under forced memory compaction, then `tickertape verify`): no reader reported corruption and every retained
  record verified, the evidence that closed issue #43, but every run failed after 1,075–1,304 s with the disk full,
  because retention removed almost nothing there; the cause is not known yet (the same command without forced
  compaction kept the journal at its limit for 10 minutes; issue #45, open). The operating envelope now covers
  1,024 B and states that open point. The roadmap's status lines,
  `docs/compatibility.md` → Before 1.0, `docs/crash-contract.md` → Known risks and limits and ADRs 0026, 0038 and
  0044 point to the results. The first session's definition of the checkpoint "pause" now matches what `tt-sustained`
  measures (the longest stretch of the writer thread's work while a checkpoint is in flight, not only the request).
- Fixed (bench tooling): `benches/session.py --only compare` failed with `FileNotFoundError` on `<out>/logs/compare.log`
  when no tickertape run had created `<out>/logs` first; the session now creates it before any run.

- Docs: ADR 0042 (replay and recovery at scale) is accepted after the cloud run of `tt-scale` (informal figures in
  pull request #33): no sparse index for 1.0, since a seek costs at most one scan of the segment holding its target
  (`docs/operations.md` → Choosing a segment size); the sparse index, a bounded read of the newest segment at writer
  open and fewer per-segment opens stay possible later work. `status::Sampler` with `Status::breaches`, and creating
  reader beacons, stay in Rust only in 1.0. The "Before 1.0" list of `docs/compatibility.md` now holds the ABI version
  and baseline freeze, the 1.0 fixtures, the release-candidate exercise and recording the sustained session's
  results; the roadmap's status lines are updated to pull request #40.
- Background checkpoints (ADR 0038) reach the C ABI and the bindings, and the iceoryx2 recorder uses them:
  - Added (C ABI): `tickertape_writer_request_checkpoint`, `tickertape_writer_checkpoint_progress` with the
    extensible struct `tickertape_checkpoint_progress` (minimum size 56), `tickertape_writer_options_set_early_writeback`,
    `tickertape_writer_options_set_helper_cpus` and the enum `tickertape_helper_cpus`. A background failure is
    returned once by the next request, progress, checkpoint or close, then `TICKERTAPE_E_DURABILITY_LOST`, as in Rust.
  - Changed (C ABI): `tickertape_writer_close` behaves as `Writer::close`: it waits for background checkpoints in
    flight and returns a failure no call has returned yet, or `TICKERTAPE_E_DURABILITY_LOST` if the writer is
    poisoned (after a failed checkpoint or a failed clean-up of an aborted fill). It returned `TICKERTAPE_OK` in those
    cases before. It still frees the handle.
  - Added (C++): `Writer::request_checkpoint`, `Writer::checkpoint_progress`, `CheckpointProgress`,
    `WriterOptions::early_writeback`, `WriterOptions::helper_cpus` and `HelperCpus`; `Writer::close` reports what the
    C close reports.
  - Added (Python): `Writer.request_checkpoint`, `Writer.checkpoint_progress`, `CheckpointProgress`, and the
    `early_writeback` and `helper_cpus` options. Changed: `Writer.close()` raises a background failure not raised yet,
    or `DurabilityLostError` for a poisoned writer (it closed silently before); leaving a `with` block raises it only
    if the block did not raise.
  - Added (Java): `Writer.requestCheckpoint`, `Writer.checkpointProgress`, `CheckpointProgress`, `HelperCpus`,
    `WriterOptions.earlyWriteback` and `WriterOptions.helperCpus`. Changed: `Writer.close()` throws a background
    failure, or `DurabilityLostException` for a poisoned writer.
  - Changed (iceoryx2 recorder): the periodic checkpoints are background requests, so recording no longer pauses
    while one syncs; the checkpoint at a stop stays blocking. A failed background checkpoint stops the recorder with
    `Error::Journal`. `RecorderReport::checkpoints` counts the requests and the final checkpoint.
- Fixed: a writer held one open file per segment after its checkpoint marker until the next checkpoint: one file and
  one segment mapping per roll, and one file per segment recovery scanned. A writer that checkpointed rarely or never
  failed with "Too many open files" after about as many rolls without a checkpoint as its descriptor limit, and
  `Writer::open` failed the same way on a journal with more segments than that after its marker (found by `tt-scale`,
  ADR 0042). v0.4.0 has the same defect. A writer now keeps only the four most recent dirty segments open, also in
  recovery, in background checkpoints and in the early write-back helper; the checkpoint that covers an older one
  reopens and syncs it (on macOS also `msync`s it through a new mapping), and fails, poisoning the writer, if it is gone
  or replaced by another file under its name (device and inode are checked). The writer makes the journal directory
  absolute at open for the files it opens later, so a writer opened with a relative path keeps using its own journal
  after the process changes its working directory (`Writer::dir` still returns the path as given). Under
  `RecoveryPolicy::Fail` the changes held back after a restored header no longer keep a file open each. Limit: for a
  segment it no longer holds, a checkpoint does not report a write-back error that the kernel forgot or that another
  descriptor already observed (crash contract → Segments the writer no longer holds; ADR 0038 → Bounded open segments).
- The "Before 1.0" list of `docs/compatibility.md` is resolved except for what the release itself does (ADR 0043,
  now accepted, effective at 1.0):
  - Changed (Rust API): the `format` module's recovery helpers `nonzero_end`, `find_history_after_empty`,
    `LaterHistory` and `SEAL_SEARCH_RECORDS` are crate-private. The scanning helpers `SegmentView`, `read_slot`,
    `Slot`, `scan_segment`, `ScanResult` and `ScanEnd` (released in v0.4.0) stay public for the workspace's tests and
    fuzz targets but are `#[doc(hidden)]` and not covered by the compatibility promise. The struct-like variants of
    `Error`, `CorruptReason` and `RecoveryAction` are `#[non_exhaustive]`: patterns need `..`, and only the crate
    builds them. A hidden `samples` module and hidden `Error::io` and `Error::corrupt` give the tests of the C ABI
    and the Python bindings one value of each variant (not covered either).
  - Added (C ABI): `TICKERTAPE_HEAD_OTHER`, `TICKERTAPE_BEACON_OTHER` and `TICKERTAPE_READERS_DIR_OTHER` (255), where
    a state the core adds later lands; before, such a state was reported as `TICKERTAPE_HEAD_INVALID`,
    `TICKERTAPE_BEACON_CHANGING` or `TICKERTAPE_READERS_DIR_UNUSABLE`. C++ `HeadEnd::other`, `BeaconState::other`
    and `ReadersDirState::other`; Java `HeadEnd.OTHER`, `BeaconState.OTHER` and `ReadersDirState.OTHER` (255) replace
    `UNKNOWN` (-1); Python reports `"other"` instead of `"unknown"` for `Head.end` and `readers_dir_state`. A build
    from one source tree returns none of them.
  - Changed (CLI): `verify --json`, `inspect --json`, `stats --json` and `archive --json` start with
    `"schema_version":1`, as `status --json` does; `docs/cli.md` states the rule (fields may be added; removing one
    or changing its meaning bumps the command's version). Tests assert it for each command, and `archive --json`'s
    `base_seq`, `size_bytes` and `sealed_unix_ns`.
  - Changed (bindings, unreleased names): the bindings use the Rust name in their own case convention:

    | Binding | Before | Now |
    |---|---|---|
    | Java | `DiscardedHistoryException` | `DiscardedException` |
    | Java | `SequenceUnavailableException` | `SeqUnavailableException` |
    | Java | `UnsupportedFormatVersionException` | `UnsupportedVersionException` |
    | Java | `JournalLockedException` | `LockedException` |
    | Java | `JournalIoException` | `JournalIOException` |
    | Java | `HeadEnd.UNKNOWN`, `BeaconState.UNKNOWN`, `ReadersDirState.UNKNOWN` (-1) | `.OTHER` (255) |
    | Python | `tickertape.status(path, readers_dir=None)` | `tickertape.JournalStatus.sample(path, readers_dir=None)` |
    | Python | `Writer.append_many` | `Writer.append_batch` |
    | Python | `Retention.archive(directory, ...)` | `Retention.move_to(directory, ...)` |
    | Python | `Retention.archive_dir` | `Retention.move_to_directory` |
    | Python | `"unknown"` in `Head.end` and `readers_dir_state` | `"other"` |

  - Decisions: ADRs 0029 (the PyPI project name `tickertape`), 0032, 0033, 0037 (Maven coordinates
    `io.github.thomas0903:tickertape`), 0038 and 0043 are accepted (by the maintainers under the owner's delegation,
    2026-11-20); ADR 0042 stays proposed. The zero-copy reader's known limitation ships in 1.0 as a documented
    limitation. What remains before 1.0: ADR 0042, the features no binding reaches yet (background checkpoints
    through the C ABI and the bindings, `Sampler`, beacons), the ABI version and baseline freeze, the 1.0 fixtures
    and the release-candidate exercise.

- Docs: `docs/compatibility.md`, the 1.0 compatibility contract (ADR 0043, **proposed**): for the Rust API (its public
  items by module, what is `#[non_exhaustive]`, what is test-only and not covered, the MSRV policy), the C ABI, the
  C++ header, the Python and Java bindings, the CLI's machine-readable outputs and exit statuses, the on-disk format
  (ADR 0033; upgrading and downgrading) and the crash contract, what is promised from 1.0 and how each may change
  afterwards, and a "Before 1.0" list of what still blocks declaring them stable. The roadmap marks the
  interoperability layer done and says for each open item what is missing.
- Docs: `c-api.md` lists all six extensible structs and their minimum sizes (it named two); `python.md` says the
  bindings are not stable before 1.0; ADR 0034's status says accepted (its pull request was merged).

- Replay and recovery at scale (ADR 0042, **proposed**): a new benchmark tool, `tt-scale` (`benches/README.md`),
  builds a journal of a given segment count, segment size and payload and times, each in its own process, listing,
  reader open, seeks to the first, middle and last record, replay, `verify`, a retention plan, and writer open after a
  clean close, after a `SIGKILL` and without a checkpoint marker, with CPU time, page faults, peak RSS and whether the
  page cache was dropped, as JSON. ADR 0042 records what each cost grows with and proposes, without building them, a
  rebuildable sparse index for seeks, a bounded read of the newest segment's unwritten part at writer open, and fewer
  per-segment opens in retention.
- Changed: `Reader::open` and `Reader::seek` pick their segment in one pass over the journal directory instead of
  collecting and sorting every segment's path, so their memory no longer grows with the number of segments. The
  segment chosen and the errors are unchanged. `docs/operations.md` now says how seek time grows with the segment
  size and which operations grow with the segment count.
- Batched append (ADR 0041): `Writer::append_batch` appends an iterator of `NewRecord`s in order and returns the
  range of sequence numbers they got. Each record is committed, and visible to readers, as it is written, as with
  `append`. Instead of one wake-up after every record, a batch that commits at least one record publishes one
  wake-up (when wake-ups are enabled) after the last record it appended, plus one at each roll inside the batch.
  That saves the writer a wake-up system call per record while a reader is blocked, and delays a blocked reader's
  view of a batch's first records until the batch ends or the next roll in it. A record that does not fit rolls, as
  with `append`. On a failure the batch stops and returns a `BatchError` with the number of records appended (they
  stay committed), the first sequence number and the cause. The C ABI's `tickertape_writer_append_batch` (signature
  unchanged) and Python's `Writer.append_batch` now use it; the C function's `*appended_out` is documented as the
  count when it returns. `WriterConfig::notify`'s documentation says what a batch publishes. The `append` benchmark
  gains an `append/batch` group. `demo-producer` encodes each event directly into its record with
  `Writer::append_with` (new `Event::encode_into` and `Event::encoded_len` in the examples crate).
- Diagnostics: `docs/playbooks/diagnose-latency-with-ebpf.md`, a Linux playbook for finding where the writer and
  reader threads wait during sustained runs or in an application (off-CPU time by kernel stack, block I/O, page
  faults, write-back throttling, run-queue latency), with bpftrace, bcc and libbpf-tools commands, requirements,
  how to line traces up with `tt-sustained`'s `series.jsonl` and with checkpoints and rolls, how to measure the
  tracing overhead and confirm with tracing off, and the investigation that led to rolls without a sync (ADR 0038).
  Scripts in `benches/diagnostics/`: `offcpu.bt`, `events.bt`, `stack-sampler.py` (no eBPF), `find-threads.sh`,
  `align.py` and `trace-sustained.sh`. Optional; nothing in the library or the CLI changes.

- Background checkpoints (ADR 0038, **proposed**):
  `Writer::request_checkpoint` starts a checkpoint of every record appended so far on the writer's checkpoint thread
  (`tickertape-checkpoint`, started by the first request) and returns the sequence number it covers up to, so appends
  continue while the sync calls run; `Writer::checkpoint_progress` returns a `CheckpointProgress` (the durable marker,
  and the request still pending); `Writer::close` waits for the checkpoints in flight and reports whether every
  checkpoint of the writer succeeded. Requests made while one checkpoint runs join into one more. A failed background
  checkpoint poisons the writer and is returned once by the next of these calls or `checkpoint()`. `checkpoint()`
  keeps its meaning (it waits for background checkpoints first), dropping the writer waits for requested checkpoints,
  and the writer's retention runs on the checkpoint thread after each background checkpoint. No format change. The
  crash contract names the background path in "Two levels", "What `checkpoint()` does", "Errors are sticky", C1, C7,
  C12, C13 and C14. Fault injection gains `Point::CheckpointBeforeMarker` and `fault::hold_background`.
  `tt-sustained --checkpoint-mode blocking|background` (and `benches/session.py --checkpoint-mode`) compares both, with
  new result fields `checkpoint_mode`, `checkpoint_durable_latency`, `checkpoint_pause` and
  `checkpoint_requests_while_in_flight`.
- Changed (ADR 0038): a roll no longer syncs the new segment on the writer's thread (on ext4 that sync waited for a
  journal commit at every roll, and behind a background checkpoint's write-back). The next checkpoint syncs every
  segment linked before its request, before its marker, so what a checkpoint covers is unchanged. After an OS crash
  or power loss, a segment after the marker can have its name on disk and not its header: recovery writes that
  header again under both policies where the scan enters the segment (or replaces a file holding only zeros whose
  length is not a valid segment size), reported as the new `RecoveryAction::RestoredHeader` (Python:
  `"restored_header"`); until a writer has opened the journal, readers and `verify` report that header as invalid
  (crash contract C8, C11). A file it replaces is replaced by renaming a prepared copy over it, so an interrupted
  open leaves no gap, and under `Fail` an open that refuses restores no header (next entry). The format is
  unchanged. Fault injection gains `Point::RollAfterLink` and `Point::RestoreBeforeRename`.
- Fixed (ADR 0038, crash contract C8, C11): under `RecoveryPolicy::Fail`, an open that restored a segment's header
  (or replaced an empty successor) and sealed the segment before it could still refuse at a later segment, such as a
  disconnected one, leaving the refused journal changed (found by the `recover_dir` fuzz target). From the first
  header it would restore, `Fail` now holds back that change and every later one the scan decides (zeroing, seals,
  created successors) and makes them, in order, only once the open can no longer refuse. A refused open makes none of
  them and removes what it prepared under temporary names. `Quarantine` is unchanged. Fault injection gains
  `Point::BetweenHeldChanges`. New fuzz seed `unwritten_successor_then_disconnected_fail`.
- Library: early write-back on Linux, `WriterConfig::early_writeback` (default on, with `prefault`): a helper thread
  (`tickertape-writeback`) starts the kernel's write-back of the whole pages behind the write position with
  `sync_file_range(SYNC_FILE_RANGE_WRITE)` every 1 MiB and of a segment the writer rolls past, so checkpoints find
  little dirty data. It makes nothing durable; an I/O error from it poisons the writer (`metrics().poisoned` at once)
  and is returned once by the next `request_checkpoint`, `checkpoint_progress`, `checkpoint` or `close` (which joins
  the helper first). A helper that stopped (that error, or the call unsupported) holds no segment file and is handed
  none. A forked child's `Writer::release_after_fork` closes its descriptors of the files the helper follows (unless
  the fork came while the helper held its lock or was starting a write-back). No effect on macOS. Fault injection
  gains `Point::EarlyWriteback`.
- Fixed: `retention::apply` (`tickertape archive`) syncs the checkpoint marker after reading the slot that decides a
  removal and before removing anything, so a power loss right after a writer wrote a new marker slot, and before its
  marker barrier, can no longer bring back a marker that names a segment the archiver removed (crash contract C14).
  Fault injection gains `Point::RetentionMarkerSync`.
- Library: `WriterConfig::helper_cpus(HelperCpus)` places the writer's helper threads (checkpoint, prefault,
  write-back, spare). Default `HelperCpus::Process`: on Linux each helper sets its affinity when it starts to every
  CPU the process's cpuset allows except the kernel's isolated CPUs, so a writer thread pinned to one CPU no longer
  gets its helpers on that CPU; `HelperCpus::Inherited` keeps the creating thread's mask (the previous behaviour),
  `HelperCpus::List` names CPUs. No effect on macOS. `tt-sustained` gains `--no-early-writeback` and `--helper-cpus`
  (so pinned runs need no `taskset`) and records the system's dirty and write-back page cache per second on Linux
  (`dirty_bytes`, `writeback_bytes`; maxima in `writer.page_cache`).
- Operational metrics through the C ABI and the bindings (ADR 0032 → Bindings; ADR 0028 amended), additively: new
  symbols only, nothing on the append or per-record read paths.
  - C ABI: `tickertape_journal_status_sample` (an immutable sample handle that several threads may read at once,
    freed by `tickertape_journal_status_free`), `tickertape_journal_status_get_report`, `_get_beacon` and
    `_beacon_name`, `tickertape_writer_get_metrics` and `tickertape_reader_get_metrics`; the extensible structs
    `tickertape_journal_status_report`, `tickertape_beacon_status`, `tickertape_writer_metrics` and
    `tickertape_reader_metrics`; the enums `tickertape_head_end`, `tickertape_checkpoint_problem`,
    `tickertape_age_unknown`, `tickertape_lag_unknown`, `tickertape_beacon_state` and `tickertape_readers_dir_state`;
    `TICKERTAPE_UNKNOWN` (`UINT64_MAX`) for values a sample could not establish. `tickertape_reader_get_metrics` does
    not invalidate a record view (the documentation of `tickertape_reader_next` and `tickertape_record` says so).
  - C++: `tickertape::JournalStatus::sample`, `StatusReport`, `BeaconStatus`, `Writer::metrics`,
    `Reader::metrics(const JournalStatus &)`, and the matching enumerations. `Checkpoint` moved up in the header
    (unchanged).
  - Python: `tickertape.JournalStatus.sample(path, readers_dir=None)`, `Writer.metrics()`,
    `Reader.metrics(status)`, and their named tuples.
  - Java: `JournalStatus.sample`, `Writer.metrics()`, `Reader.metrics(JournalStatus)`, `WriterMetrics`,
    `ReaderMetrics`, `BeaconStatus`, `Position`, `Lag` and the enums `HeadEnd`, `CheckpointProblem`, `AgeUnknown`,
    `LagUnknown`, `BeaconState`, `ReadersDirState`.
  - Not exposed outside Rust yet: `Sampler` (rates, time to full), `Status::breaches` and creating reader beacons.

- Release operations: install channels, part 2 (ADR 0031). Every new publishing step is switched off until the owner
  enables it (repository variables `PYPI_CLI_PUBLISHING` and `MAVEN_CENTRAL_PUBLISHING`, environments `pypi-cli` and
  `maven-central`; the release checklist has the one-time set-up), and none runs for pull requests or `main`.
  - The CLI as a wheel, PyPI project `tickertape-cli` (`pipx install tickertape-cli`,
    `uvx --from tickertape-cli tickertape`): maturin's `bin` bindings, manylinux and musllinux (x86-64, aarch64) and
    macOS (x86-64, arm64), built in `python-wheels.yml`, which installs each wheel on its runner (musllinux in
    Alpine) and runs `--version`, `verify` and `tail` on a released journal fixture. Published with Trusted
    Publishing from the environment `pypi-cli`, then installed from PyPI with pipx.
  - A C library archive per platform (`tickertape-c-vX.Y.Z-<target>.tar.gz`, glibc Linux x86-64 and aarch64, macOS
    x86-64 and arm64): the shared library with its soname or install name (`libtickertape.so.0.4`,
    `@rpath/libtickertape.0.4.dylib` before 1.0), the static library, `tickertape.h`, `tickertape.hpp`, the
    relocatable pkg-config files `tickertape.pc` and `tickertape-static.pc`, licences and a README. New
    `release.py c-build` and `c-package`; `crates/tickertape-ffi/scripts/install-test.sh` builds and runs the C and
    C++ examples from the unpacked archive through pkg-config, shared and static, in CI (`c abi` job), in the
    release workflow and against the published release.
  - Maven Central for the Java binding (`io.github.thomas0903:tickertape`): the main, sources and Javadoc JARs and
    four natives JARs, whose libraries come from the C archives. CI builds them unsigned with every change (new
    Maven profile `release`; the `natives` profile builds one JAR per staged platform) and runs the install test.
    The release workflow signs them with gpg and uploads them through the Central Portal API (`release.py
    maven-bundle`, `maven-upload`) in a job that runs no build code and is skipped while the `maven-central`
    environment's secrets are absent, then compares every file on Central with the build and runs the install test
    with the downloaded JARs. `stage-natives.sh` now stages into `target/natives/<platform>`, and
    `install-test.sh` takes an optional JAR pair. The pom gains `developers`, `scm` and `issueManagement`.
  - Build provenance and CycloneDX SBOM attestations for every new file: the Python bindings' wheels and sdist, the
    CLI wheels, the C archives (whose SBOMs are also release assets) and the Maven files. `release.py sbom` takes
    `--package` (`cli`, `c-library`, `python`, `java`) and `--name`; `release.py prepare` also checks the Java
    binding's version against the tag.

- Fix: `tickertape export` under retention or archiving (crash contract C14). Without `--from`, the writing pass
  opened the journal again and started at whatever segment was oldest then, so retention between the startup scan and
  the writing pass shifted the exported window (exit 0 with the wrong records) or failed with "journal changed during
  export"; it now starts at the record the startup scan started at. A segment that recovery replaced or retention
  removed after a pass had read it ended the export with exit 1 ("the segment at sequence N was replaced or removed");
  both passes now continue at that position on the current chain. Records removed before export read them end it with
  exit 1 and `export stopped at sequence N: records N to M were removed from the journal (by retention or archiving)
  before export read them` (`record N was ... read it` for one), after the records before N (a Parquet file is
  finalized, as on corruption). The check that the output file is not a journal file no longer fails (exit 1, "stat
  journal entry") on a segment removed while it walks the directory; it skips that entry unless the inode number in
  its directory entry is the output's.
- Fix: `tickertape export` could exit 0 with records from a different history: if recovery replaced records the
  startup scan had counted and the writer appended others with the same sequence numbers before the writing pass
  read them (crash contract C8, C11), the writing pass exported the other records. Both passes now compute a
  fingerprint of the records they read (sequence number, schema id, timestamps, payload length and bytes); when they
  differ, export exits 1 with `journal changed during export: the records read differ from those the startup scan
  counted; the output is not a consistent export`, after writing what the writing pass read (a Parquet file is
  finalized).
- Fix: `tickertape export` no longer creates or truncates the destination when it finds the failure before writing
  the first record: a startup scan that failed before counting a record, and a first record that is gone by the time
  the writing pass reads it (with `--durable-only`, retention after the writing pass's `seek`), used to leave an empty
  file (or an empty Parquet file) with exit 1. The writing pass now reads its first record before opening the
  destination.
- Fix: `tickertape export --to T` with `T` below the oldest record read a record outside the range, so retention at
  that moment turned an empty export (exit 0) into exit 1 naming records outside it; it now checks `--to` before
  reading. With `--durable-only`, a checkpoint marker that became missing or invalid during the startup scan ended
  the scan early and exported fewer records (or none) with exit 0; that is now exit 1 with `journal changed during
  export: the checkpoint marker no longer covers records it covered when export started`, before the destination is
  opened.
- Library: `Writer::try_append_with(&mut self, record, payload_len, fill)`, a zero-copy append whose closure
  (`FnOnce(&mut [u8]) -> Result<(), E>`) can decline the record by returning `Err(e)`; the call then returns
  `Ok(Err(e))`, and journal errors stay in the outer `Result<_, tickertape::Error>`. Declining does what a panic in an
  `append_with` closure does (nothing committed, the bytes zeroed and the zeroing synced) without unwinding (ADR 0039).
  If that sync fails, the writer stops (`abandoned_write_error`) and the call returns the outer `Err`, the same
  `Error::Io` as every later append, dropping the caller's `e`: an outer `Err` means the journal failed, and `fill` did
  not run except in that case. `append_with` and `append` now go through it; a panic in either closure behaves as
  before.
- C ABI: `tickertape_writer_append_with` aborts an append whose `fill` returns non-zero through `try_append_with`,
  without unwinding inside the library (ADR 0039). Its signature, status codes and documented behaviour are
  unchanged. Before, `libtickertape.a` linked by the GCC driver on macOS (Homebrew GCC, which passes
  `-no_compact_unwind`) ended the process at the first aborted fill; that link now works, and the tests no longer skip
  it. A Rust panic inside the library still unwinds (`TICKERTAPE_E_PANIC`). A test builds the static library with
  `panic = "abort"` (through a private compiler cfg in that build's own rustflags, not a Cargo feature) and runs the
  basic C test, including an aborted fill, against it.
- Fixed: writer recovery no longer zeroes the bytes after an empty commit word in place when a segment for the
  expected sequence number exists but does not open (an invalid header, for example) and recovery finds no committed
  history after the word (ADR 0035 → Damage around a seal, crash contract C8 and C11 (3); it was recorded there as a
  known gap). The chain is broken there, and those bytes can be the only ones left for diagnosis or salvage. With
  `RecoveryPolicy::Quarantine` they are now preserved in `quarantine/` and the segment is replaced, as for any
  corrupt tail (`RepairedTail` with `CorruptReason::BrokenChain`); then that next segment and the later ones are
  moved to `quarantine/` as disconnected, as before. With `RecoveryPolicy::Fail` the open is refused with
  `BrokenChain` at the empty commit word before any byte of that segment changes; it used to zero them, then refuse
  at the next segment. With nothing non-zero after the word nothing changes. `verify` already reported this state as
  `BrokenChain` at the same position. The on-disk format is unchanged.
- Changed: when the segment that follows an empty commit word exists but cannot be opened for a reason other than
  its content (an I/O error such as a permission or a failed mapping; also a directory or a dangling link under a
  segment's name), `Writer::open` now fails with `Error::Io` without changing anything at that place, under both
  recovery policies (what recovery did earlier in the same scan stays). Only a segment that is invalid (`Error::Corrupt`) or of an unsupported version ends the chain there.
  Until now any failure to open it ended the chain: after a plain crash during a roll, a transient error could make
  `Quarantine` move that healthy segment and every later one to `quarantine/` (crash contract C11).
- Internal: the readers decode a record's header straight into their own state and pass only a small tag and the
  length along the per-record path; the rare outcomes (empty slot, seal, failed validation, checkpoint boundary) are
  handled outside it. Behaviour is unchanged. This undoes a slowdown for small records that the recovery change
  (ADR 0035) had brought in on the way to this release (no released version had it): the per-record code had not
  changed, but the compiler had started to copy the decoded header through memory in a way the CPU could not
  forward. `CONTRIBUTING.md` now asks for the `replay` benchmark before and after a change to the code that path is
  built from.
- C++ binding (ADR 0036, `docs/cpp-api.md`): `crates/tickertape-ffi/include/tickertape.hpp`, a header-only C++17
  wrapper over the C ABI, shipped and installed (`cargo cinstall`) next to `tickertape.h`. It covers the whole C ABI:
  move-only `Writer`, `Reader`, option builders and `RecordBatch`; copying, zero-copy (a lambda) and batch appends
  (`NewRecord` is the C struct `tickertape_new_record`, built with `tickertape::new_record`); checkpoint; seek; next
  with `std::chrono` timeouts; durable-only reads; the zero-copy reader as `Reader::open_mapped_unsafe`; `RecordView`s
  whose borrowed payload is checked in debug builds; copy-out (into any memory or a writable byte range) and batch
  reads; verify, segment listing, the checkpoint marker and retention. Errors are values (`tickertape::result<T>`,
  `[[nodiscard]]`, with a `Status`, message, OS error and corruption reason) whose `value()` throws, so it works with
  and without `-fno-exceptions`; a temporary result or error never lends a reference, and `tickertape::Exception`
  copies without throwing (moving one copies it, so a moved-from exception keeps its error). An exception thrown by a
  fill never crosses the C ABI: it is rethrown after the aborted append, nested inside the library error if the
  clean-up failed, and destroying or moving the writer inside its own fill is defined. Negative timeouts do not wait
  (unlike C), and integral durations are converted to nanoseconds exactly, rounded up (for every period the
  standard library can convert to nanoseconds; a more extreme one does not compile); a negative or
  NaN retention age is an error. With GCC and Clang, units built with and without exceptions or debug checks can be
  linked into one program, and binding types cannot be passed between units built in different exception modes: the
  inline namespace is `tickertape::v1` with exceptions and `tickertape::v1_noexcept` without, so every definition the
  header provides has a mode-specific symbol (and it instantiates no throwing standard-library function over only its
  C types); a binding type in the parameter list of a C++ function called directly by name makes a crossing a link
  error, but one only in a non-template function's return type, in an `extern "C"` function, or passed through a
  virtual function or a function pointer, links and is undefined, so the rule is not to cross. Standard-library code
  over standard types stays
  shared, as in any program that mixes modes, and so does a container your own code instantiates over `NewRecord` or
  another C type: that is as safe as the toolchain's standard library makes it (CI runs the mixed-mode test with GCC
  and Clang on Linux and Apple Clang on macOS; Homebrew GCC on macOS was run by hand). The debug-check-dependent
  functions carry an `abi_tag`.
  `new_record` refuses temporaries that own their bytes at compile time; rvalue views are accepted where
  `tickertape::enable_borrowed_bytes` is true (`std::basic_string_view`, `std::span`, and your own view types if you
  specialise it), the same at every language level. Paths are passed directly; the parameter type is the internal
  `tickertape::detail::PathArg`, which protects the documented call forms from a dangling temporary but not code that
  names it. Nothing the header defines depends on the language level. Every translation unit of a program must use the
  same release of the header; no class gains a data member within a major version. The header checks the C header's
  version at compile time and the library's in every call that takes a journal path and in the option builders before
  they allocate. Tested as C++17, C++20 and C++17 without exceptions under AddressSanitizer, UBSan and Valgrind,
  compile-tested as C++17/20/23 with and without exceptions and `NDEBUG` with GCC and Clang under `-Wconversion
  -Wshadow -Wpedantic`, with a layout pin for every class, `static_assert`s and compile-fail programs for the API's
  compile-time properties, a check that the header's definitions are the same as C++17, C++20 and C++23, mixed-mode
  link tests with every compiler (failing each allocation inside a binding function in turn, and an `nm` check that
  the two exception modes define no common binding symbol), an allocation check for the hot path with a positive
  control and cross-language fixtures; tests fail if a C function, struct field, status or corruption reason is added
  without a decision for the binding. A CMake package (`tickertape::tickertape`, `tickertape::cpp`) serves
  `find_package` and `add_subdirectory`. The C++11 example wrapper `examples/c/tickertape.hpp` is removed;
  `examples/c/example.cpp` now records, follows live and replays with the binding, and the examples need C++17. On
  macOS, link the static library with Apple's toolchain defaults or use the shared library (`docs/c-api.md`).
- Java binding (ADR 0037, `docs/java-api.md`): `bindings/java`, a Maven project (JDK 25, module
  `io.github.thomas0903.tickertape`, no runtime dependencies) over the C ABI with the Foreign Function and Memory API
  and no JNI. It covers the writer (copying, zero-copy and batch appends, checkpoint), the safe and mapped readers
  (try, timed and blocking next, batch reads, seek, durable-only), inspection and retention. Use after close,
  concurrent calls on a handle, a stale record view and a payload buffer used on another thread throw instead of
  crashing; closing a reader from another thread is safe while a view is being read (views read through the reader's
  shared arena, which close ends first); blocking reads honour `Thread.interrupt()` and a close from another thread
  without losing records; record views are reused and checked, keep their memory alive, and the hot paths allocate
  nothing (checked with the JIT's escape analysis off too). A close refuses new calls from the moment it starts and
  takes over from the call in progress without a gap; closing a reader costs a handshake with every JVM thread, so
  readers are meant to be long-lived. A path is passed to the library by the bytes the JDK holds for it (a relative
  path stays relative), so a name the JVM cannot decode opens the journal of that name and not one of a similar
  name; only the default file system is supported. What remains undefined (another thread advancing a reader while its view is read, sharing a batch between
  threads, the mapped reader's precondition) is listed in the guide. Reports from growable C structs are classes, not
  records, so they can gain fields after 1.0.
  Every C status and corruption reason has its own exception or constant, checked against the header. The library is
  found by explicit path, system property, a natives JAR (extracted privately and hash-checked) or the library path,
  and a library of another version is refused. Tested on Linux x86-64, Linux arm64 and macOS in a new CI job, including
  the cross-language fixtures and the released-format fixtures. Not published to Maven Central yet; the coordinates
  `io.github.thomas0903:tickertape` await the owner's confirmation.
- Fixed: a visible reader no longer reports `CorruptReason::BrokenChain` for a seal that is no longer in the file.
  When recovery repairs a tail in place, a seal can go with it while the successor is moved to `quarantine/`. The
  safe reader could still hold the seal in its read buffer (a zero-copy reader could load it just before the damage
  or the repair), follow it, find no successor while its own segment kept its identity, and report a broken chain on
  a healthy journal on every call, until the new writer rolled to the same base again or the application seeked. A
  reader at a seal whose successor it does not find now reads the slot again from the file, and reports a broken
  chain only if the seal is still there and the successor is still missing at a second attempt. Found by
  `reader_retention_stress::durable_readers_under_repairs` on CI (arm64 Linux and macOS, three times in one night),
  where one zeroed commit word in a sealed segment caused such a repair; with ADR 0035 recovery replaces that
  segment instead. A seal can still disappear from a file that recovery leaves in place (storage damage that zeroes
  the whole seal in front of a successor that no longer opens, or a stretch from a record through the seal), and the
  reader now handles it. A failed refill of the safe reader's buffer also drops the buffer, so the next call reads from the
  file.
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
  from a torn append and is zeroed in place as before (crash contract C8, C11). A torn record whose payload holds bytes the search counts as committed history (raw
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
- Integration: `tickertape-iceoryx2` 0.1.0 in `integrations/iceoryx2` (ADR 0030; outside the workspace, own lockfile and
  version, not published). Records one or several iceoryx2 publish/subscribe services into a journal (a descriptor per
  service, repeated so that every segment holds the descriptors of the records in it (a filler record ends a segment
  early when needed) and retained segments decode on their own, then one record per message with publisher id, user
  header and payload; layout version 1 documented byte by byte, with a growth rule and validity rules), with periodic
  and final checkpoints, clean SIGINT/SIGTERM handling (default actions restored between runs), per-service failures
  that are reported and retried, and loss records from an optional publisher-side counter. Every size, alignment,
  element count and service setting read from a journal or file is validated before it reaches iceoryx2. Replays from
  any sequence number onto renamed services (`replay/` by default), as fast as possible or at the recorded pace with
  late-send reports, refusing type changes on one target and names the journal is recording live; typed replay is safe,
  replay by recorded type details is an `unsafe fn`. Replay targets and recorder subscribers are refused before
  their ports exist if the service's settings in force would make iceoryx2 allocate more than documented limits.
  Exports to and imports from iceoryx2's human-readable recording files with a writer and parser of its own (imports
  all or nothing; the binary representation is not supported). A typed recorder and replayer use only iceoryx2's public API;
  recording services by name (feature `dynamic`, used by the `tickertape-iox2` tool) uses API iceoryx2 marks internal,
  so iceoryx2 is pinned to `=0.10.0`. CI builds and tests it on Linux x86-64, Linux aarch64 and macOS, and weekly
  (informational) against the newest iceoryx2 release and upstream `main`.
- Dependencies: `deny.toml` allows the 0BSD licence (public-domain equivalent; `enum-iterator`, required by iceoryx2,
  in the integration's lockfile).
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
- Release operations: install channels, part 1 (ADR 0031). Releases add static Linux CLI archives
  (`x86_64-unknown-linux-musl`, `aarch64-unknown-linux-musl`) that need no C library, built and tested natively;
  cargo-binstall metadata in `tickertape-cli` (Linux resolves to the static archive; CI checks the templates against
  the archive names); a Homebrew tap (`brew install thomas0903/tap/tickertape`) updated by the release workflow;
  build provenance attestations for every archive and SBOM, a CycloneDX SBOM per archive (attached and attested),
  and the dependency list embedded in the binary with `cargo auditable`. The release workflow verifies each
  channel by installing from it. mise is documented. A backfill run now compares every existing asset before
  uploading anything.
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
