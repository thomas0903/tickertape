# 0033: Format-v1 compatibility fixtures and the soak tier

- **Status:** proposed (test strategy for the v1.0 readiness milestone; the compatibility promise needs the owner's
  agreement)
- **Date:** 2026-11-19

## Context

The on-disk format has been version 1 since v0.1.0 (`docs/format.md`), and every release since has said "the format
is unchanged". Nothing checked that claim against journals the released versions actually wrote: the tests only
write journals with the current code. Readers in the field may also be older than the writer that produces their
journal. The roadmap's v1.0 milestone asks for fixtures from released versions and for longer-running crash,
corruption, retention and concurrency tests, including on aarch64 Linux.

## Options considered

1. **Byte layouts written by hand in the tests.** Small, but they encode what we think the old writers did, not what
   they did (e.g. the auxiliary files a version leaves behind, or a torn append's bytes).
2. **Journals written by each tag's own writer, committed (chosen).** `tests/compat/generate.sh` builds the tag's
   library from `git archive` of the tagged commit with that tag's `Cargo.lock`, and a small generator
   (`tests/compat/generator/main.rs`, using only API that exists since v0.1.0) writes them. Committed fixtures don't
   change when the code does, which is the point.
3. **Generate old journals in CI on every run.** Always fresh, but it builds four old versions per run and the
   tested bytes would not be reviewable.

For the storage of preallocated segments: compression would need a dependency in the tests, so the fixtures use a
small pack format (`tests/compat/pack.py`) that stores every byte except long runs of zeros, with each file's exact
length. The manifest records every file's SHA-256 as written, before packing; the tests check them after unpacking,
so the packing cannot hide or change a byte.

## Decision

**What "format v1 compatible" is tested to mean.** For every released tag (v0.1.0 to v0.4.0), on Linux aarch64,
Linux x86-64 and macOS aarch64, a journal closed cleanly and one whose writer was killed in the middle of an append
(64 KiB segments, 4 segments, a maximum-size record, empty and varied payloads, extreme schema ids and timestamps, a
checkpoint in the middle, and the `notify` and `next-segment.tmp` files those versions leave behind):

- *New code reads old journals* (every pull request, `tests/compat.rs`, `crates/tickertape-cli/tests/compat_fixtures.rs`):
  the safe and mapped readers return exactly the recorded record stream (by digest), `durable_only` readers stop at
  the recorded checkpoint, `inspect::verify` and `read_checkpoint` report the recorded segments and marker; the CLI's
  `verify`, `inspect`, `export --format jsonl` and `stats` and the Python decoder agree.
- *New code continues old journals*: the current `Writer` opens a copy, recovers as the crash contract says (the torn
  append's bytes zeroed, the stale spare removed, nothing quarantined), appends across rolls, checkpoints, and the
  result verifies and is gap-free; a corrupt record after the checkpoint is quarantined byte for byte, or refused with
  `RecoveryPolicy::Fail` without changing a byte.
- *Old code reads new journals* (every pull request on Linux x86-64, weekly on Linux aarch64 and macOS:
  `tests/compat/generate.sh --check-old-readers`): each tag's readers (and its `durable_only` readers, v0.2.0+) and
  `verify` see exactly what the working tree's writer wrote, clean and torn. This is cheap (about 30 s), so it runs.

Not covered: journals written with other segment sizes or by other platforms than the three above, and old *writers*
continuing new journals (a downgrade of the writer); v1.0 does not promise that.

**Every release adds its fixtures** (`docs/playbooks/release-checklist.md`), and `tests/compat.rs` fails if a tag in
its `RELEASED_TAGS` list lacks Linux and macOS fixtures. The fixture set must stay under 1 MiB (about 0.5 MiB for the
first 24).

**Soak tier** (`tests/soak.rs`). One test that combines the failure modes for a configurable time instead of testing
them one at a time: writer kills (including during recovery), rolls, checkpoints, retention moving segments to an
archive, injected storage-full roll failures and checkpoint failures, corruption injected between restarts, and six
long-lived reader processes plus late ones. It reconstructs the expected history (which writer incarnation wrote each
sequence number, and which records recovery removed) and checks every reader's log against it. A 20 s smoke runs in
the pull-request long tests; a 90-minute run per platform runs weekly (`.github/workflows/soak.yml`).

## Consequences

- A change that alters what old journals mean fails in pull requests, against bytes no one can regenerate by
  accident. If a format change is ever intended, it needs a new format version and new fixtures (`docs/format.md` →
  Versioning); the existing fixtures then document what version 1 readers must keep accepting.
- The compatibility tests take about 5 s; the soak smoke adds about 20 s to each long-test job; the old-readers job
  runs in parallel with the others.
- The fixtures' platforms are recorded in the manifest: macOS ones on APFS; Linux aarch64 ones in an OrbStack VM
  (btrfs); Linux x86-64 ones in an emulated (Rosetta) x86-64 container. Fresh native x86-64 fixtures can be added with
  the same script.
- The soak injects corruption by renaming a modified copy over a segment, because writing into a file a mapped reader
  has mapped is outside `Reader::open_mapped`'s safety contract. Storage-full episodes are injected (the
  `fault-injection` feature), because real full volumes need `hdiutil` or root (`tests/disk_full.rs`).
