# Playbook: run the fuzz targets

The fuzz crate is in `fuzz/` and needs a nightly toolchain and `cargo-fuzz`:

```bash
rustup toolchain install nightly
cargo install cargo-fuzz
```

## Targets

| Target | What it feeds arbitrary bytes to |
|---|---|
| `parse_segment` | Segment header validation and the forward recovery scan over an in-memory segment image |
| `decode_record` | Single-record header and frame decoding |
| `recover_dir` | Writer crash recovery on real directories: the input describes 1 to 4 segment files (valid records, seals, overwritten, zeroed or flipped bytes, forged CRCs, damaged headers, short files), an optional checkpoint marker, stray `*.tape.tmp` files and a `quarantine/` directory. Runs `Writer::open` with `Quarantine` or `Fail` and checks the invariants listed below |

List them with `cargo +nightly fuzz list`.

## Runs

```bash
# quick smoke run (CI does this for 60 s per target)
cargo +nightly fuzz run parse_segment -- -max_total_time=60

# the M4 exit criterion: at least 10 minutes per target with no crash
cargo +nightly fuzz run parse_segment -- -max_total_time=600
cargo +nightly fuzz run decode_record -- -max_total_time=600
# recover_dir writes files and needs its seeds (run from fuzz/); -max_len keeps inputs small
cargo +nightly fuzz run recover_dir corpus/recover_dir seeds/recover_dir -- -max_total_time=600 -max_len=1024
```

### `recover_dir`

Unlike the other two targets, this one writes files: it builds each journal in a fresh `tempfile` directory (so it
honours `TMPDIR`) and runs about 190 executions per second on a Mac (see "Recorded runs"), mostly in `fsync`, `mmap` and file creation.
The input grammar is documented at the top of `fuzz/fuzz_targets/recover_dir.rs`. After `Writer::open` it checks:

1. `open` never panics (an error is fine).
2. If it succeeds, `inspect::verify` reports no problems, a safe `Reader` reads a gap-free run of records that ends at
   `Writer::next_seq`, and one appended record is readable.
3. Opening again succeeds and repairs nothing (idempotent), and still ends at the same `next_seq`.
4. Nothing is deleted: every `RepairedTail` and `QuarantinedSegment` left exactly one file in `quarantine/` holding the
   removed bytes, bytes before a repair are unchanged, and a file only disappears if it was quarantined.
5. With `RecoveryPolicy::Fail`, a refused `open` changed no committed byte, and a successful one quarantined nothing.
   It may still have zeroed never-committed bytes, sealed a segment or created a missing successor, because
   `docs/format.md` → Recovery does that before a later corrupt position or a disconnected segment is found.

Recovery trusts everything before a checkpoint marker and does not re-read it, so problems that `verify` finds there
are not findings, and a marker whose segment does not hold valid records up to the position it names only gets the
checks that do not depend on the marker.

The seeds are produced by `fuzz/examples/gen_recover_seeds.rs` (`cd fuzz && cargo run --example gen_recover_seeds`);
see what a seed does with `RECOVER_DIR_TRACE=1 cargo +nightly fuzz run recover_dir seeds/recover_dir/<name>`
(prints the recovery report or the error).

## Seeds and corpus

Seed inputs are in `fuzz/seeds/<target>/` (committed). The working corpus (`fuzz/corpus/`) and crash artifacts
(`fuzz/artifacts/`) are gitignored. Start a run with seeds: `cargo +nightly fuzz run parse_segment fuzz/corpus/parse_segment fuzz/seeds/parse_segment`.

## Recorded runs

| Date | Target | Commit | Machine | Duration | Executions | Result |
|---|---|---|---|---|---|---|
| 2026-10-31 | `parse_segment` | `96d23e0` | owner's Mac (Apple M4 Pro, macOS 27) | 601 s | 179,748,260 | no crash |
| 2026-10-31 | `decode_record` | `96d23e0` | owner's Mac (Apple M4 Pro, macOS 27) | 601 s | 109,819,500 | no crash |
| 2026-11-01 | `recover_dir` | `d6c26c0` | Apple M4 Pro, macOS 27.0.1, `$TMPDIR` on APFS | 601 s | 111,394 (185/s) | no crash (earlier runs found the `Fail` bug fixed in `be94c8f`) |
| 2026-11-01 | `parse_segment` | `d6c26c0` | Apple M4 Pro, macOS 27.0.1 | 61 s | 24,705,109 | no crash |
| 2026-11-01 | `decode_record` | `d6c26c0` | Apple M4 Pro, macOS 27.0.1 | 61 s | 27,320,337 | no crash |

## When it finds a crash

1. Minimise it: `cargo +nightly fuzz tmin parse_segment fuzz/artifacts/parse_segment/<crash-file>`.
2. Add the minimised input as a regression unit test in `crates/tickertape` (a byte-array literal), and check it fails.
3. Fix the bug, re-run the target, and record the run (target, duration, executions, result) in "Recorded runs" above.
