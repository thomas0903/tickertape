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

List them with `cargo +nightly fuzz list`.

## Runs

```bash
# quick smoke run (CI does this for 60 s per target)
cargo +nightly fuzz run parse_segment -- -max_total_time=60

# the M4 exit criterion: at least 10 minutes per target with no crash
cargo +nightly fuzz run parse_segment -- -max_total_time=600
cargo +nightly fuzz run decode_record -- -max_total_time=600
```

Seed inputs are in `fuzz/seeds/<target>/` (committed). The working corpus (`fuzz/corpus/`) and crash artifacts
(`fuzz/artifacts/`) are gitignored. Start a run with seeds: `cargo +nightly fuzz run parse_segment fuzz/corpus/parse_segment fuzz/seeds/parse_segment`.

## When it finds a crash

1. Minimise it: `cargo +nightly fuzz tmin parse_segment fuzz/artifacts/parse_segment/<crash-file>`.
2. Add the minimised input as a regression unit test in `crates/tickertape` (a byte-array literal), and check it fails.
3. Fix the bug, re-run the target, and record the run (target, duration, executions, result) in `docs/status.md`.
