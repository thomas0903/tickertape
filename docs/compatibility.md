# Compatibility

What tickertape promises to keep working from 1.0 on, for each surface, and how each may change after 1.0. The policy
is [ADR 0043](decisions/0043-compatibility-policy.md) (accepted, effective at 1.0). Each section points to the
document that holds the rules; this page does not restate them. The surfaces below are stable from 1.0.0
(2026-11-23); releases 0.x could change any of them, and the changelog says when they did.
[Before 1.0](#before-10) records how the last open points were resolved.

| Surface | Versioned by | Rules |
|---|---|---|
| Rust crate `tickertape` | SemVer of the crate | [Rust API](#rust-api) |
| C ABI (`libtickertape`, `tickertape.h`) | `TICKERTAPE_ABI_VERSION`, soname | [`c-api.md`](c-api.md#versioning), ADR 0028 |
| C++ header (`tickertape.hpp`) | the C ABI's version | [`cpp-api.md`](cpp-api.md), ADR 0036 |
| Python bindings (`tickertape` on PyPI) | the release version | [Python and Java](#python-and-java), ADR 0029 |
| Java binding (`io.github.thomas0903:tickertape`) | the release version, the C ABI's version | [`java-api.md`](java-api.md), ADR 0037 |
| CLI (`tickertape`) | the `tickertape-cli` version; `schema_version` in each `--json` output | [CLI](#cli), [`cli.md`](cli.md#machine-readable-contracts) |
| On-disk format | `format_version` (1) | [On-disk format](#on-disk-format), [`format.md`](format.md#versioning), ADR 0033 |
| Crash contract | this policy | [Crash contract](#crash-contract), [`crash-contract.md`](crash-contract.md) |

All of these are released together from one tag and carry the same version number (the Python bindings take theirs
from `Cargo.toml`, and `release.py prepare` checks the Java binding's against the tag;
`docs/playbooks/release-checklist.md`). The iceoryx2 integration
(`integrations/iceoryx2`, ADR 0030) has its own version and is not covered by this page.

## What a 1.x release may and may not do

From 1.0, a release that breaks a stable surface is a new major version (2.0). A 1.x release may add: public items,
variants of `#[non_exhaustive]` enums, fields of `#[non_exhaustive]` structs, C functions, constants, status codes,
enumerators and fields at the end of extensible structs, binding methods and classes, CLI commands and options, and
JSON fields. It does not remove or rename any of these, change a documented meaning, or weaken a guarantee of the
crash contract. An item to be removed is first deprecated (`#[deprecated]` in Rust, a note in the changelog and the
binding's documentation) and stays until the next major version. Bug fixes that make the code do what the documents
say are not breaking changes, even when a program relied on the bug.

## Rust API

The public items of the `tickertape` crate at this commit, by module, as rustdoc lists them. **NE** marks
`#[non_exhaustive]`: a struct you cannot build with a literal or destructure without `..`, an enum you match with a
wildcard arm. Methods are on docs.rs.

| Module | Items |
|---|---|
| crate root | `Writer`, `Reader`, `Records`, `Record`, `OwnedRecord`, `NewRecord`, `Error` (NE), `CorruptReason` (NE), `Result`, `WriterConfig` (NE), `ReaderConfig` (NE), `Backoff` (NE), `RecoveryPolicy` (NE), `HelperCpus` (NE), `BatchError` (NE), `CheckpointProgress` (NE), `RecoveryAction` (NE), `RecoveryReport` (NE), `VERSION` |
| `clock` | `realtime_ns` |
| `inspect` | `list_segments`, `read_checkpoint`, `verify`, `SegmentInfo` (NE), `SegmentReport` (NE), `SegmentEnd` (NE), `VerifyReport` (NE), `Problem` (NE) |
| `latency` (from 1.1) | `Recorder`, `View`, `Snapshot`, `Bucket` (NE), `RecordAge`, `RecordAgeView`, `RecordAgeSnapshot` (NE) |
| `retention` | `plan`, `apply`, `Policy` (NE), `Plan`, `PlannedSegment` (NE), `Action` (NE), `Reason` (NE), `Report` (NE) |
| `seek_index` | `build`, `check`, `remove`, `DIR_NAME`, `BuildReport` (NE), `CheckReport` (NE), `SegmentIndex` (NE), `IndexState` (NE) |
| `status` | `status`, `Sampler`, `ReaderBeacon`, `READERS_DIR`, `DEFAULT_BEACON_INTERVAL`, and the sample's types, all NE: `Status`, `Head`, `HeadEnd`, `Position`, `Cursor`, `Segments`, `CheckpointStatus`, `CheckpointProblem`, `AgeUnknown`, `Exposure`, `Storage`, `Rates`, `WriterProcess`, `BeaconState`, `BeaconDirState`, `BeaconReport`, `ReaderStatus`, `Lag`, `LagUnknown`, `WriterMetrics`, `Thresholds`, `Breach` |
| `format` | constants (`MAGIC`, `CHECKPOINT_MAGIC`, `FORMAT_VERSION`, the `*_LEN` lengths, `SEAL_MARKER`, `MAX_FRAME_LEN`, `RECORD_ALIGN`, `SEGMENT_SIZE_UNIT`, `MAX_SEGMENT_SIZE`, `SEGMENT_EXTENSION`, `CHECKPOINT_FILE_LEN`, `CHECKPOINT_SLOT_OFFSETS`); header and slot types `SegmentHeader`, `RecordHeader`, `CheckpointSlot`; encoders and decoders (`encode_segment_header`, `decode_segment_header`, `encode_record_body`, `finish_record_body`, `write_record_fields`, `decode_record_body`, `seal_crc`, `encode_checkpoint_slot`, `decode_checkpoint`, `frame_len`, `align8`, `max_payload_len`, `record_fits`, `validate_segment_size`, `segment_file_name`, `parse_segment_file_name`) |

The struct-like variants of `Error`, `CorruptReason` and `RecoveryAction` (such as `Error::Corrupt { .. }`) are NE
too: match them with `..`; only the crate builds them. `Plan`, `Sampler`, `ReaderBeacon`, `Writer`, `Reader`,
`Records` and the `latency` types other than the two marked NE have only private fields. The bucket layout of the
latency histograms may change; `Snapshot::buckets` gives the bounds. `Record`, `OwnedRecord` and `NewRecord` have
public fields and are not NE: they hold exactly what a format-version-1 record holds, so they are frozen with it.

**Not covered:** the `fault` module and everything behind the `fault-injection` feature (test-only; `Cargo.toml` says
never to enable it in production), items compiled only under `cfg(fuzzing)` or `cfg(loom)`, `#[doc(hidden)]` items
(the segment scanning helpers `format::SegmentView`, `read_slot`, `Slot`, `scan_segment`, `ScanResult` and `ScanEnd`,
public for the workspace's tests and fuzz targets; the byte codec `seek_index::codec` and `Reader::seek_used_index`, for
the same; the `samples` module and `Error::io` and `Error::corrupt`, for the
tests of the C ABI and the Python bindings), the wording of `Display` and error messages, and `Debug` output. Error
*variants* and `CorruptReason` values are covered; the strings are not. The other workspace crates (`tickertape-ffi`
is `publish = false`; tests, benches, examples) and the library half of `tickertape-cli` (`market_demo`, the decoder
the `tickertape` binary uses) are not Rust API.

**MSRV.** 1.85 (`rust-version` in `Cargo.toml`, checked by CI's `msrv` job). From 1.0 a minor release may raise it,
with a changelog entry; a patch release does not.

## C ABI

[`c-api.md`](c-api.md#versioning) and ADR 0028 hold the rules: ABI version 0 before 1.0 (load exactly the
library your header came from), ABI version 1 and soname `libtickertape.so.1` from 1.0; additions only within ABI
version 1.

- **The baseline check.** `crates/tickertape-ffi/abi/tickertape-baseline.h` is the header shipped in 1.0.0 (it
  declared nothing before 1.0). At every later stable release it becomes that release's header, and
  `.github/scripts/check_abi_baseline.py` fails CI if a declaration in it is removed or changed (`docs/playbooks/release-checklist.md` → C ABI at 1.0).
- **Extensible structs** start with `uint64_t struct_size`; the library fills `min(struct_size, its own size)` and
  writes back what it filled, so a newer program can tell which fields an older library wrote. Seven structs are
  extensible (`c-api.md` → Versioning lists them with their minimum sizes). The record structs,
  `tickertape_segment_info` and `tickertape_checkpoint` describe format version 1 and are frozen.
- **Status codes and corruption reasons** keep their values; new ones may be added. Treat an unknown negative status
  as an error. `TICKERTAPE_E_UNKNOWN` (-255) and `TICKERTAPE_CORRUPT_UNKNOWN` (255) stand for an error or reason the
  library's ABI version has no code for.
- **`TICKERTAPE_UNKNOWN` and the `_OTHER` values.** `TICKERTAPE_UNKNOWN` (`UINT64_MAX`) in a metrics field means the
  sample could not establish the value; it is not a forward-compatibility marker. Every metrics enum has a value 255
  (`TICKERTAPE_HEAD_OTHER`, `TICKERTAPE_CHECKPOINT_OTHER_PROBLEM`, `TICKERTAPE_AGE_OTHER_REASON`,
  `TICKERTAPE_LAG_OTHER_REASON`, `TICKERTAPE_BEACON_OTHER`, `TICKERTAPE_READERS_DIR_OTHER`) where a value the core
  added after the C mapping was written would land; a library built from one source tree does not return them,
  since its tests name every core variant. A program should handle them as "unknown" (for the head, not as healthy).
- **Not in the C ABI:** `status::Sampler` (rates, time to full), `Status::breaches`, creating reader beacons and the
  `latency` module; they stay in Rust only for now and may be added in 1.x ([Before 1.0](#before-10)). `c-api.md` → Operational
  metrics says so.

## C++ header

`tickertape.hpp` is header-only over the C ABI and has no ABI of its own: its inline code is compiled into each
program. [`cpp-api.md`](cpp-api.md) → Versioning holds the rules, including that every part of a program must be
rebuilt on upgrade, because two parts compiled against different headers can end up running one header's inline
code. From 1.0, source compatibility follows the [1.x rules](#what-a-1x-release-may-and-may-not-do): no class,
member or enumerator is removed or renamed. C++17 is the minimum standard.

## Python and Java

- **Python** (ADR 0029): the public API is what `tickertape.__all__` lists (classes, exceptions, functions and
  constants), with the documented behaviour in [`bindings/python/README.md`](../bindings/python/README.md) and
  [`python.md`](python.md). Names starting with `_` (such as `_staging_capacity_for_testing`) are not covered.
  The bindings use the Rust crate directly, not the C ABI. CPython 3.11 is the minimum; from 1.0 a minor release
  may raise it with a changelog entry. The read-only decoder `python/tickertape_reader.py` follows the format
  (format version 1); its own Python API is not covered.
- **Java** (ADR 0037): the public types of the exported package `io.github.thomas0903.tickertape`; the
  package-private classes (`Native`, `NativeLibrary`, `Memory`, `Errors` and others) are not covered.
  [`java-api.md`](java-api.md) → Versioning holds the library-version check (exactly the binding's version before
  1.0; same ABI version and at least as new from 1.0). JDK 25 is the minimum.

Where the bindings name the same thing, they use the Rust name in their own case convention (`DiscardedError`,
`DiscardedException`; `append_batch`, `appendBatch`; `JournalStatus.sample`). The differences that remain are
idiomatic: exception suffixes, prefixes where a bare name would read as a built-in (`JournalIOError`,
`JournalNotFoundError`), and Python's string codes where C, C++ and Java have enums. The renames made before 1.0 are
in the changelog.

## CLI

Machine-readable and covered, as specified in [`cli.md`](cli.md#machine-readable-contracts):

- record JSONL (`tail --format json`, `export --format jsonl`), with its fixed key order;
- `verify --json`, `inspect --json`, `stats --json`, `archive --json`, `index --json` (from 1.1), `age --json` (from 1.1), `status --json`
  (including the `--watch` error line) and `status --format prometheus` (metric names, labels and meanings);
- the Parquet schema of `export --format parquet` and the columns `--decode market-demo` adds;
- exit statuses: 0 success, 1 usage, I/O or missing journal, 2 corruption or unsupported format (for `status`, also
  a journal the writer would refuse), 3 a `status` threshold breached (`cli.md`, first section).

From 1.0, JSON outputs may gain fields (consumers must ignore fields they do not know, and read objects by name,
except record JSONL, whose key order is fixed) and enumerated string values where `cli.md` says a set may grow (such
as `inspect`'s `end_state` `"unknown"`); no field is removed or changes meaning in 1.x. Every `--json` output
(`verify`, `inspect`, `stats`, `archive`, `index`, `status`, `age`) starts with `"schema_version":1` and keeps it throughout
1.x. Prometheus metrics may be added, not renamed or removed. A new exit status may appear only with a new command or
option.

**Not covered:** text output (`tail` without `--format json`, `inspect`, `verify`, `stats`, `status` and `age` without
`--json`), everything on standard error (warnings, error messages), the `reason` and `message` strings inside JSON
(`cli.md` calls them human-readable diagnostics), and `--help` text.

## On-disk format

The format is version 1 ([`format.md`](format.md); `format_version` changes whenever a version 1 reader could
misread a file written by new code, `format.md` → Versioning). What "compatible" is tested to mean is ADR 0033: on
Linux x86-64, Linux aarch64 and macOS, with the fixture shapes the ADR lists,

- **new code reads and continues old journals:** journals written by every released version (v0.1.0 to v0.4.0, v0.5.1 and v1.0.0) are
  read identically by the current readers, CLI and Python decoder, and the current writer recovers and continues
  them as the crash contract says (`tests/compat.rs`, `crates/tickertape-cli/tests/compat_fixtures.rs`);
- **old code reads new journals:** each released version's readers and `verify` read what the current writer wrote,
  clean and after a kill (`tests/compat/generate.sh --check-old-readers`).

From 1.0: every 1.x release reads, recovers and continues every format-version-1 journal, and every 1.x writer
writes format version 1, so 1.0's readers can read what a later 1.x writer wrote. Every release adds its fixtures
(release checklist). Not promised, as ADR 0033 says: an older *writer* continuing a newer writer's journal, and
journals from other platforms or segment sizes than the fixtures'. The beacon files in `readers/` (ADR 0032) are not
part of format version 1 and have their own version byte; nor are the seek indexes in `seek-index/` (ADR 0045), which
have a version of their own, can be deleted at any time, and are ignored by 1.0.0
(`tests/compat/v1.0.0-ignores-seek-index.sh`; the one exception is that 1.0.0's `export --output` fails with exit 1, and
must be run again, if it cannot list `seek-index/` or the directory vanishes while it runs). A 1.0.0 writer does not
remove indexes before it repairs a journal: readers that use them then rely on the checks by value alone (crash contract
C15).

**Upgrading and downgrading.**

- *Upgrade:* no migration. Close the writer (a journal whose writer did not close is recovered at the next open as
  after a crash, crash contract C1/C2), install the new version and open the journal with it. Readers
  and CLI commands of either version can read the result. Tested: old readers on journals the new writer left.
  Not tested: an old reader process following a new writer live.
- *Downgrade of readers or the CLI:* covered by "old code reads new journals" above, back to v0.1.0.
- *Downgrade of the writer:* not promised and not tested. In particular, after an OS crash or power loss, a journal
  written by the current writer can contain a segment whose header never reached the disk; the current writer
  restores it at open (ADR 0038 → Rolls without a sync, `RecoveryAction::RestoredHeader`), but older writers do not
  know that state. Until a writer of the current version has opened such a journal, readers and `verify` of any
  version see that segment's header as invalid.

## Crash contract

[`crash-contract.md`](crash-contract.md) is part of the contract: each guarantee, per platform and durability
level, with the test that checks it, and the stated known limitations and risks. The exact recovery rules are its
C1, C2, C8 and C11 and ADR 0035. From 1.0 a 1.x release may strengthen a guarantee or narrow a known limitation (with
an ADR); weakening a guarantee, or turning an untested claim into a weaker one, is a breaking change.

The zero-copy reader's known limitation ships in 1.0 as a documented limitation, part of the contract
(`crash-contract.md` → "Readers, the writer and memory safety" → "Known limitation (commit words damaged on
storage)"; ADR 0027 → "Which loads can race"): a commit word with a value that no writer stored there can make the
reader load bytes a writer is storing or, if it is zero, let recovery rewrite published records in place under a live
reader. (The text of 1.0.0 named less of this damage. Naming it is a correction, not a weakening: all of it breaks the
intact chain on which the guarantee depends.) [ADR 0048](decisions/0048-publication-frontier.md) (accepted, not built)
would narrow the first, by the checkpoint marker, in a 1.x release.

## Before 1.0

What had to be resolved, or explicitly accepted, before the surfaces above were declared stable, and how each point
was closed for 1.0.0. The rest of the
list this review produced was resolved on 2026-11-20 (ADR 0043 → Consequences), and so were two later lines: ADR 0042
is accepted (no sparse index for 1.0; the sparse index stays possible later work), and `status::Sampler` (rates, time
to full) with `Status::breaches`, and creating reader beacons, stay in Rust only in 1.0 (decided by the maintainers
under the owner's delegation, 2026-11-20). A program using a binding can compute rates from two status samples (each
has its sampling time, the head's next sequence number, the segments' total bytes and the storage headroom) and
compare them with its own thresholds; any of the three can be added to the C ABI and the bindings in 1.x.

1. The C ABI at 1.0: done in the 1.0.0 release preparation. `TICKERTAPE_ABI_VERSION` is 1 and the ABI baseline
   (`crates/tickertape-ffi/abi/tickertape-baseline.h`) is the header shipped in 1.0.0, with 177 frozen declarations
   (`docs/playbooks/release-checklist.md` → C ABI at 1.0).
2. The 1.0 fixtures: added on 2026-11-23, written by the released version on macOS and Linux aarch64
   (`docs/playbooks/release-checklist.md`, ADR 0033), as for every release.
3. The roadmap's release-candidate exercise (release, installation and upgrade instructions and the external
   integration against a release candidate): done with v0.5.1 on 2026-11-23, the first release through every
   channel (`docs/playbooks/release-checklist.md` → What the v0.5.1 release found). Each channel was installed from
   after publication, the v0.4.0 fixture journals are read and continued by the released code, and the iceoryx2
   integration's tests pass against the crate from crates.io. Four defects were found and fixed, all in steps
   that until then ran only for a release tag. The exercise's manual checks were repeated for 1.0.0 itself after its
   publication.
4. The sustained session: its results and the operating envelope are recorded in `docs/benchmarks.md` (2026-11-19 to
   2026-11-22) for both payloads, the 1,024 B scenarios with background checkpoints at `3fea1c0` (every run finished).
   The record checksum mismatch of the 1,024 B probe at `279f59a` is resolved (issue #43, closed after the long verified
   runs; the platform mechanism is not confirmed). Retention behind an overloaded writer is resolved (issue #45, closed;
   ADR 0044, amendment of 2026-11-23): long runs with background checkpoints that complete, each journal verified from
   the file afterwards, are recorded at `fbe5785` (`docs/benchmarks.md` → Confirming long runs). The library's code
   (`crates/tickertape/src`) is unchanged between `fbe5785` and 1.0.0, so that run was not repeated.
