#!/usr/bin/env bash
# Runs the benchmark set recorded in docs/benchmarks.md and writes raw output plus environment details to OUTDIR.
# Usage: benches/run-recorded.sh OUTDIR    (journals go under $TMPDIR; run from the repository root, at a clean commit)
set -euo pipefail
OUT="${1:?usage: benches/run-recorded.sh OUTDIR}"
mkdir -p "$OUT"
WORK="$(mktemp -d "${TMPDIR:-/tmp}/tt-bench.XXXXXX")"
BIN="${CARGO_TARGET_DIR:-./target}/release"

{
  echo "date_utc: $(date -u +%FT%TZ)"
  echo "commit: $(git rev-parse HEAD)$(git diff --quiet && echo '' || echo ' (DIRTY)')"
  echo "rustc: $(rustc --version)"
  echo "uname: $(uname -a)"
  echo "journal_dir: $WORK"
  if [[ "$(uname)" == Darwin ]]; then
    sw_vers; sysctl -n machdep.cpu.brand_string hw.ncpu hw.memsize; diskutil info / | grep -E "File System Personality|Solid State|Device / Media Name"
    df -h "$WORK" | tail -1
  else
    lscpu | grep -E "Model name|^CPU\(s\)|Architecture"; free -g | head -2; df -T "$WORK" | tail -1
    [[ -e /run/.containerenv || "$(uname -r)" == *orbstack* ]] && echo "environment: OrbStack VM (Linux on macOS)"
  fi
} > "$OUT/environment.txt" 2>&1

cargo build --release -p tickertape-bench --bins
# Delete each run's journal and flush the page cache's write-back before the next run: otherwise gigabytes written
# by earlier runs are still being written back during later ones and distort them (seen on ext4, 2026-11-01).
clean() { rm -rf "${WORK:?}"/*; sync; }
for run in 1 2; do
  for payload in 64 1024; do
    for reader in mapped safe; do
      $BIN/tt-latency run --dir "$WORK/lat-$run-$payload-$reader" --rate 100000 --count 1100000 --warmup 100000 \
        --payload "$payload" --reader "$reader" --json >> "$OUT/latency.jsonl"
      clean
    done
    $BIN/tt-latency run --dir "$WORK/lat-$run-$payload-mapped-noprefault" --rate 100000 --count 1100000 --warmup 100000 \
      --payload "$payload" --reader mapped --no-prefault --json >> "$OUT/latency.jsonl"
    clean
    for wait in default sleep; do
      $BIN/tt-latency run --dir "$WORK/lat-$run-$payload-safe-$wait" --rate 100000 --count 1100000 --warmup 100000 \
        --payload "$payload" --reader safe --wait "$wait" --json >> "$OUT/latency.jsonl"
      clean
    done
    $BIN/tt-checkpoint --dir "$WORK/ckpt-$run-$payload" --payload "$payload" --batch 1000 --checkpoints 200 \
      --segment-size 16777216 --json >> "$OUT/checkpoint.jsonl"
    clean
    $BIN/tt-latency run --dir "$WORK/cold-$run-$payload" --rate 1000000 --count 1000000 --payload "$payload" \
      --reader safe --cold --json >> "$OUT/cold.jsonl" || true
    clean
  done
done
TMPDIR="$WORK" cargo bench -p tickertape-bench --bench append -- --noplot 2>&1 | tee "$OUT/criterion-append.txt" >/dev/null
TMPDIR="$WORK" cargo bench -p tickertape-bench --bench replay -- --noplot 2>&1 | tee "$OUT/criterion-replay.txt" >/dev/null
echo "done: $OUT"
