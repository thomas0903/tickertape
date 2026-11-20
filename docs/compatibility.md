# Compatibility

What tickertape promises to keep working from 1.0 on, for each surface, and how each may change after 1.0. The
policy is [ADR 0043](decisions/0043-compatibility-policy.md) (proposed). Each section points to the document that
holds the rules; this page does not restate them. Nothing below is stable yet: before 1.0 any release may change
any surface, and the changelog says when. [Before 1.0](#before-10) lists what still has to be resolved before these
surfaces can be declared stable.

| Surface | Versioned by | Rules |
|---|---|---|
| Rust crate `tickertape` | SemVer of the crate | [Rust API](#rust-api) |
| C ABI (`libtickertape`, `tickertape.h`) | `TICKERTAPE_ABI_VERSION`, soname | [`c-api.md`](c-api.md#versioning), ADR 0028 |
| C++ header (`tickertape.hpp`) | the C ABI's version | [`cpp-api.md`](cpp-api.md), ADR 0036 |
| Python bindings (`tickertape` on PyPI) | the release version | [Python and Java](#python-and-java), ADR 0029 |
| Java binding (`io.github.thomas0903:tickertape`) | the release version, the C ABI's version | [`java-api.md`](java-api.md), ADR 0037 |
| CLI (`tickertape`) | the `tickertape-cli` version; `schema_version` for `status` | [CLI](#cli), [`cli.md`](cli.md#machine-readable-contracts) |
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
| `retention` | `plan`, `apply`, `Policy` (NE), `Plan`, `PlannedSegment` (NE), `Action` (NE), `Reason` (NE), `Report` (NE) |
| `status` | `status`, `Sampler`, `ReaderBeacon`, `READERS_DIR`, `DEFAULT_BEACON_INTERVAL`, and the sample's types, all NE: `Status`, `Head`, `HeadEnd`, `Position`, `Cursor`, `Segments`, `CheckpointStatus`, `CheckpointProblem`, `AgeUnknown`, `Exposure`, `Storage`, `Rates`, `WriterProcess`, `BeaconState`, `BeaconDirState`, `BeaconReport`, `ReaderStatus`, `Lag`, `LagUnknown`, `WriterMetrics`, `Thresholds`, `Breach` |
| `format` | constants (`MAGIC`, `CHECKPOINT_MAGIC`, `FORMAT_VERSION`, the `*_LEN` lengths, `SEAL_MARKER`, `MAX_FRAME_LEN`, `RECORD_ALIGN`, `SEGMENT_SIZE_UNIT`, `MAX_SEGMENT_SIZE`, `SEGMENT_EXTENSION`, `CHECKPOINT_FILE_LEN`, `CHECKPOINT_SLOT_OFFSETS`, `SEAL_SEARCH_RECORDS`); header and slot types `SegmentHeader`, `RecordHeader`, `CheckpointSlot`; encoders and decoders (`encode_segment_header`, `decode_segment_header`, `encode_record_body`, `finish_record_body`, `write_record_fields`, `decode_record_body`, `seal_crc`, `encode_checkpoint_slot`, `decode_checkpoint`, `frame_len`, `align8`, `max_payload_len`, `record_fits`, `validate_segment_size`, `segment_file_name`, `parse_segment_file_name`); scanning (`SegmentView`, `read_slot`, `Slot`, `scan_segment`, `ScanResult`, `ScanEnd`, `nonzero_end`, `find_history_after_empty`, `LaterHistory` (NE)) |

`Plan`, `Sampler`, `ReaderBeacon`, `Writer`, `Reader` and `Records` have only private fields. `Record`, `OwnedRecord`
and `NewRecord` have public fields and are not NE: they hold exactly what a format-version-1 record holds, so they
are frozen with it.

**Not covered:** the `fault` module and everything behind the `fault-injection` feature (test-only; `Cargo.toml`
says never to enable it in production), items compiled only under `cfg(fuzzing)` or `cfg(loom)`, `#[doc(hidden)]`
items, the wording of `Display` and error messages, and `Debug` output. Error *variants* and `CorruptReason` values
are covered; the strings are not. The other workspace crates (`tickertape-ffi` is `publish = false`; tests, benches,
examples) and the library half of `tickertape-cli` (`market_demo`, the decoder the `tickertape` binary uses) are not
Rust API.

**MSRV.** 1.85 (`rust-version` in `Cargo.toml`, checked by CI's `msrv` job). From 1.0 a minor release may raise it,
with a changelog entry; a patch release does not.

## C ABI

[`c-api.md`](c-api.md#versioning) and ADR 0028 hold the rules: ABI version 0 before 1.0 (load exactly the
library your header came from), ABI version 1 and soname `libtickertape.so.1` from 1.0; additions only within ABI
version 1.

- **The baseline check.** `crates/tickertape-ffi/abi/tickertape-baseline.h` is empty before 1.0. At 1.0 and at every
  later stable release it becomes the shipped header, and `.github/scripts/check_abi_baseline.py` then fails CI if
  a declaration in it is removed or changed (`docs/playbooks/release-checklist.md` → C ABI at 1.0).
- **Extensible structs** start with `uint64_t struct_size`; the library fills `min(struct_size, its own size)` and
  writes back what it filled, so a newer program can tell which fields an older library wrote. Six structs are
  extensible (`c-api.md` → Versioning lists them with their minimum sizes). The record structs,
  `tickertape_segment_info` and `tickertape_checkpoint` describe format version 1 and are frozen.
- **Status codes and corruption reasons** keep their values; new ones may be added. Treat an unknown negative status
  as an error. `TICKERTAPE_E_UNKNOWN` (-255) and `TICKERTAPE_CORRUPT_UNKNOWN` (255) stand for an error or reason the
  library's ABI version has no code for.
- **`TICKERTAPE_UNKNOWN` and the `_OTHER` values.** `TICKERTAPE_UNKNOWN` (`UINT64_MAX`) in a metrics field means the
  sample could not establish the value; it is not a forward-compatibility marker. `TICKERTAPE_CHECKPOINT_OTHER_PROBLEM`,
  `TICKERTAPE_AGE_OTHER_REASON` and `TICKERTAPE_LAG_OTHER_REASON` (255) are where a value the core added after the C
  mapping was written would land; a library built from one source tree does not return them, since its tests name
  every core variant. A program should handle them as "unknown problem/reason". The enums `tickertape_head_end`,
  `tickertape_beacon_state` and `tickertape_readers_dir_state` have no such value: an unmapped core value would be
  reported as `TICKERTAPE_HEAD_INVALID`, `TICKERTAPE_BEACON_CHANGING` and `TICKERTAPE_READERS_DIR_UNUSABLE`
  (`crates/tickertape-ffi/src/metrics.rs`); see [Before 1.0](#before-10).
- **Not in the C ABI:** `status::Sampler` (rates, time to full), `Status::breaches`, creating reader beacons,
  `Writer::request_checkpoint` and `Writer::checkpoint_progress` (ADR 0038). `c-api.md` → Operational metrics says
  so for the first three.

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

The bindings name some things differently from each other and from Rust (listed in [Before 1.0](#before-10)).

## CLI

Machine-readable and covered, as specified in [`cli.md`](cli.md#machine-readable-contracts):

- record JSONL (`tail --format json`, `export --format jsonl`), with its fixed key order;
- `verify --json`, `inspect --json`, `stats --json`, `archive --json`, `status --json` (including the `--watch`
  error line) and `status --format prometheus` (metric names, labels and meanings);
- the Parquet schema of `export --format parquet` and the columns `--decode market-demo` adds;
- exit statuses: 0 success, 1 usage, I/O or missing journal, 2 corruption or unsupported format (for `status`, also
  a journal the writer would refuse), 3 a `status` threshold breached (`cli.md`, first section).

From 1.0, JSON outputs may gain fields (consumers must ignore fields they do not know, and read objects by name,
except record JSONL, whose key order is fixed) and enumerated string values where `cli.md` says a set may grow (such
as `inspect`'s `end_state` `"unknown"`); no field is removed or changes meaning in 1.x. `status --json` keeps
`schema_version` 1 throughout 1.x. Prometheus metrics may be added, not renamed or removed. A new exit status may
appear only with a new command or option.

**Not covered:** text output (`tail` without `--format json`, `inspect`, `verify`, `stats` and `status` without
`--json`), everything on standard error (warnings, error messages), the `reason` and `message` strings inside JSON
(`cli.md` calls them human-readable diagnostics), and `--help` text.

## On-disk format

The format is version 1 ([`format.md`](format.md); `format_version` changes whenever a version 1 reader could
misread a file written by new code, `format.md` → Versioning). What "compatible" is tested to mean is ADR 0033: on
Linux x86-64, Linux aarch64 and macOS, with the fixture shapes the ADR lists,

- **new code reads and continues old journals:** journals written by every released version (v0.1.0 to v0.4.0) are
  read identically by the current readers, CLI and Python decoder, and the current writer recovers and continues
  them as the crash contract says (`tests/compat.rs`, `crates/tickertape-cli/tests/compat_fixtures.rs`);
- **old code reads new journals:** each released version's readers and `verify` read what the current writer wrote,
  clean and after a kill (`tests/compat/generate.sh --check-old-readers`).

From 1.0: every 1.x release reads, recovers and continues every format-version-1 journal, and every 1.x writer
writes format version 1, so 1.0's readers can read what a later 1.x writer wrote. Every release adds its fixtures
(release checklist). Not promised, as ADR 0033 says: an older *writer* continuing a newer writer's journal, and
journals from other platforms or segment sizes than the fixtures'. The beacon files in `readers/` (ADR 0032) are not
part of format version 1 and have their own version byte.

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

## Before 1.0

What this review found must be resolved, or explicitly accepted, before the surfaces above are declared stable.
Each line ends with a recommendation. Items marked *fixed* were fixed in the pull request that added this page.

**Decisions still proposed**

1. ADR 0029 (Python bindings): the PyPI distribution name `tickertape` and the publishing step need the owner's
   agreement. Agree or rename before the first upload; a rename after it breaks every install command.
2. ADR 0032 (operational metrics, reader beacons): proposed, with no stated condition, while the `status` module,
   `tickertape status`, the beacon file layout and the C metrics structs are all public. Review and accept it
   (possibly amended); otherwise mark these surfaces as not covered.
3. ADR 0033 (format-v1 fixtures): the compatibility promise needs the owner's agreement. Agree to the promise as
   stated, including what it leaves out (writer downgrade, other platforms and segment sizes).
4. ADR 0037 (Java binding): the Maven coordinates `io.github.thomas0903:tickertape` need the owner's agreement and a
   registered Central namespace. Agree before the first upload.
5. ADR 0038 (background checkpoints): proposed, while `Writer::request_checkpoint`, `checkpoint_progress`,
   `CheckpointProgress`, `HelperCpus`, `WriterConfig::early_writeback` and `helper_cpus`, and
   `RecoveryAction::RestoredHeader` are already public. Record the Linux results in `docs/benchmarks.md` and accept
   it, then add `request_checkpoint` to the C ABI and the bindings (its Consequences say they adopt it then).
6. ADR 0042 (replay and recovery at scale): proposed until the cloud run on local SSD decides on the sparse index.
   No public API depends on it; accept or close it with that decision.
7. ADR 0043 (this policy): needs the owner's agreement, together with ADR 0033.
8. ADR 0034 said it would be accepted when its pull request merged (#14, 2026-11-19); its status line still said
   proposed. *Fixed:* it now says accepted.

**Open changes and known issues**

9. Pull request #35 (open): under `RecoveryPolicy::Fail`, an open that restores an unwritten header and then refuses
   can leave changed files (found by the `recover_dir` fuzz target); the fix rewrites crash contract C8 and C11.
   Review and merge it (or settle C11's wording) before freezing the crash contract.
10. The zero-copy reader's known limitations (crash contract → "Readers, the writer and memory safety" → "Known
    limitation (commit words damaged on storage)"; ADR 0027 → "Which loads can race"): a zeroed commit word lets recovery rewrite published records in
    place under a live reader, and the roadmap's publication frontier is not built. Decide whether 1.0 ships with
    them as documented limitations (they are then part of the contract) or after the frontier ADR.

**Rust API**

11. The `format` module exposes recovery internals as well as the byte layout: `scan_segment`, `ScanResult`,
    `Slot`, `ScanEnd`, `read_slot` and `SegmentView` (released in v0.4.0), and `nonzero_end`,
    `find_history_after_empty`, `LaterHistory` and `SEAL_SEARCH_RECORDS` (added since). Keep the constants, header
    types and encoders/decoders stable, and at 1.0 hide the scanning and recovery helpers (`#[doc(hidden)]` or
    `pub(crate)`) unless a tool needs them.
12. Released exhaustive types that could need to grow: the enums `format::Slot` and `format::ScanEnd`, and the
    struct `format::ScanResult` (public fields). Add `#[non_exhaustive]` at 1.0, or hide them (item 11).
    `SegmentHeader`, `RecordHeader` and `CheckpointSlot` are format version 1 by definition: keep them exhaustive.
13. Struct-like variants of `Error`, `CorruptReason` and `RecoveryAction` (for example `Error::Corrupt { .. }`) are not
    `#[non_exhaustive]` themselves, so adding a field to one later breaks patterns without `..`. Mark them at 1.0.
14. `format::LaterHistory`, an enum added since v0.4.0, was exhaustive. *Fixed:* it is now `#[non_exhaustive]`.

**C ABI and bindings**

15. `tickertape_head_end`, `tickertape_beacon_state` and `tickertape_readers_dir_state` have no `_OTHER` value, unlike
    the other metrics enums, so a core value added later would be reported as an existing one. Add `_OTHER = 255` to
    each before the ABI is frozen.
16. `c-api.md` listed only two of the six extensible structs and their minimum sizes. *Fixed.*
17. Names differ between bindings for the same thing: Python `DiscardedError`, `SeqUnavailableError`,
    `JournalIOError`, `UnsupportedVersionError` and Java `DiscardedHistoryException`,
    `SequenceUnavailableException`, `JournalIoException`, `UnsupportedFormatVersionException`; Python
    `tickertape.status()` and C++/Java `JournalStatus::sample`/`JournalStatus.sample`; Python `append_many` and
    Rust/C/C++/Java `append_batch`/`appendBatch`. Choose one name per concept (each language's casing) before the
    first upload to PyPI and Maven Central, when renames are still free.
18. Python had no stability statement. *Fixed:* `python.md` now says it is not stable before 1.0 and points here.
19. Rust-only features no binding reaches: `Sampler`, `Status::breaches`, creating beacons, background checkpoints
    (item 5). None is promised to the bindings; decide which 1.0 needs (each can be added in 1.x).

**CLI**

20. Only `status --json` carries a `schema_version`. The other JSON forms are covered by the additive rule above;
    keep it so, or add `schema_version` to `inspect`, `verify`, `stats` and `archive` before 1.0 (adding it later is
    allowed, but consumers cannot rely on it for 1.0 output).
21. `archive --json`: no test checks the `base_seq`, `size_bytes` and `sealed_unix_ns` fields of `segments`. Add the
    assertions to `crates/tickertape-cli/tests/archive.rs`. The other documented JSON forms, the Prometheus output
    and exit statuses 0 to 3 have tests.

**Release**

22. At 1.0: set `TICKERTAPE_ABI_VERSION` to 1, freeze the ABI baseline, add the 1.0 fixtures
    (`docs/playbooks/release-checklist.md`), and complete the roadmap's release-candidate exercise (an RC tag needs
    the owner's approval).
