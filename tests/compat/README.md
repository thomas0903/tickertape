# Format-v1 compatibility fixtures

Journals written by each released version's own writer, and the tests that read and continue them with the current
code. Why and what is promised: [ADR 0033](../../docs/decisions/0033-format-compatibility-fixtures.md).

| Path | What |
|---|---|
| `fixtures/*.ttfx` | One journal directory each, packed (format below). Binary; never edit by hand |
| `fixtures/manifest.json` | Per fixture: tag and commit that wrote it, OS, CPU, machine, `rustc`, the generator command, every file's length and SHA-256, the record count, sequence range, checkpoint, segments, and SHA-256 digests of the full and the checkpoint-covered record stream |
| `generator/main.rs` | The generator, compiled against each tag's library (only API that exists since v0.1.0) |
| `generate.sh` | Builds a tag from `git archive` in a temporary directory and writes, packs and records its fixtures; `--check-old-readers` runs each tag's readers on journals from the working tree |
| `pack.py` | Packs a journal directory, checks the round trip, and adds the manifest entry |
| `support.rs`, `sha256.rs` | Shared by the Rust tests: unpack into a temporary directory and check every file's digest |

Tests: `tests/compat.rs` (readers, `durable_only`, `verify`, writer recovery and continuation, quarantine) and
`crates/tickertape-cli/tests/compat_fixtures.rs` (CLI `verify`, `inspect`, `export --format jsonl`, `stats`, and
`python/tickertape_reader.py`). Both run in `cargo test --workspace`. They unpack every fixture into a new temporary
directory and only ever read the committed files.

## Each fixture

Segment size 64 KiB. The "clean" journal: 40 small records (payloads of 0 to 300 bytes), one record with the
largest payload that fits (65,424 bytes, alone in its segment), 30 small records, a 40,000-byte record, 10 small
records, **a checkpoint** (generation 1, at sequence 82 in the middle of the third segment), 25 small records, a
30,000-byte record and 20 small records: 128 records in 4 segments, the last one open. Schema ids include 0,
2^31 and 2^32-1; timestamps include 0, 1, 2^63, 2^64-2 and 2^64-1. Large payloads are mostly zeros, which keeps the
packs small; the CRC covers every byte.

The "torn" journal: the same plus 15 records, then an append whose payload is written but whose commit word is not:
the generator kills the writing process with SIGKILL inside `append_with`'s fill closure. It keeps whatever that
version leaves behind: `writer.lock`, `notify` (v0.3.0+), and on macOS (v0.3.0+) a prepared `next-segment.tmp`.

## Adding the fixtures for a new release

After the tag `vX.Y.Z` exists (`docs/playbooks/release-checklist.md`), on a Mac and in a Linux machine (journals on
a local filesystem; `docs/playbooks/test-on-linux.md`):

```sh
TT_COMPAT_PLATFORM_NOTE="Apple M4 Pro; journals on APFS" tests/compat/generate.sh vX.Y.Z
orb -m tickertape bash -lc 'cd "'"$PWD"'" && TMPDIR=$HOME/tmp \
  TT_COMPAT_PLATFORM_NOTE="OrbStack VM (Linux on macOS); journals on btrfs" tests/compat/generate.sh vX.Y.Z'
```

Optionally the same for Linux x86-64. Add `"vX.Y.Z"` to `RELEASED_TAGS` in `tests/compat.rs`, run
`cargo test --workspace`, and commit the new `.ttfx` files and `manifest.json` (`test: add vX.Y.Z compatibility
fixtures`). Regenerating an existing tag replaces its entries; only do that on purpose, and say why.

Existing fixtures were generated on 2026-11-19 with:

```sh
tests/compat/generate.sh v0.1.0 v0.2.0 v0.3.0 v0.4.0                         # macOS 27.2.1, Apple M4 Pro, APFS
orb -m tickertape bash -lc '… TMPDIR=$HOME/tmp tests/compat/generate.sh v0.1.0 v0.2.0 v0.3.0 v0.4.0'
                                                                             # OrbStack VM, Linux aarch64, btrfs
docker run --rm --platform linux/amd64 -v "$HOME:$HOME" -w "$PWD" -e TMPDIR=/work rust:1.99 \
  bash -c 'mkdir -p /work && git config --global --add safe.directory "*" && \
           tests/compat/generate.sh v0.1.0 v0.2.0 v0.3.0 v0.4.0'           # x86-64 under Rosetta emulation
```

The v0.5.1 fixtures were generated on 2026-11-23 with the first two commands (macOS and the OrbStack VM); there is
no Linux x86-64 set for v0.5.1.

## Pack format

`TTFXPACK` version 1, little-endian: magic, version (u32), file count (u32); per file, sorted by name: name length
(u16) and UTF-8 name, exact file length (u64), run count (u32), then runs of (offset u64, length u32, bytes). Bytes
outside the runs are zero. Runs keep every non-zero byte and zero gaps shorter than 32 bytes. `pack.py` checks that
the pack reproduces every file before writing it, and `pack.py add` checks the pack against the generator's digests;
`support.rs` checks them again in every test run.
