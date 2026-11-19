#!/bin/bash
# Checks that the released v0.4.0 tools ignore a `readers/` directory of reader beacons (ADR 0032, Compatibility):
# builds the v0.4.0 tag and this checkout's `tt-beacon-reader`, puts live and stale beacons, junk, a FIFO and a
# subdirectory into a journal's `readers/`, runs v0.4.0's writer (open and recovery), verify, inspect, archive and
# export against it, and fails unless they all succeed and `readers/` is unchanged.
#
# Usage: tests/compat/v0.4.0-ignores-readers.sh [work_dir]   (from the repository root; needs git and cargo)
# Builds into <work_dir>/target-v0.4.0 and <work_dir>/target; delete <work_dir> afterwards.
set -euo pipefail
ROOT=$(pwd)
WORK=${1:-$(mktemp -d)}
mkdir -p "$WORK/v0.4.0"
git -C "$ROOT" archive v0.4.0 | tar -x -C "$WORK/v0.4.0"
(cd "$WORK/v0.4.0" && CARGO_TARGET_DIR="$WORK/target-v0.4.0" cargo build -q -p tickertape-cli -p tickertape-tests --bins)
CARGO_TARGET_DIR="$WORK/target" cargo build -q -p tickertape-tests --bin tt-beacon-reader
OLD=$WORK/target-v0.4.0/debug
NEW=$WORK/target/debug
W=$WORK/run
rm -rf "$W"; mkdir -p "$W/dest"
J=$W/journal
run() { echo "== ${*##*/}"; "$@" > "$W/last.out"; }

run "$OLD/tt-writer" "$J" 5000 65536 64
"$NEW/tt-beacon-reader" "$J" live 4000 hold "$W/live.ready" &
LIVE=$!
trap 'kill $LIVE 2>/dev/null || true' EXIT
"$NEW/tt-beacon-reader" "$J" dead 1000 abort "$W/dead.ready" 2>/dev/null && exit 1 || true
while [ ! -e "$W/live.ready" ]; do sleep 0.05; done
echo "notes" > "$J/readers/junk"
mkfifo "$J/readers/fifo"
mkdir "$J/readers/sub"
echo x > "$J/readers/sub/f"
snapshot() {
    (cd "$J/readers" && find . -print0 | sort -z | xargs -0 ls -ld | awk '{print $1, $5, $NF}' &&
        find . -type f -print0 | sort -z | xargs -0 shasum)
}
snapshot > "$W/before"
run "$OLD/tt-writer" "$J" 6000 65536 64
# Non-zero bytes after the head of the newest segment, for the next open's recovery.
NEWEST=$(ls "$J"/*.tape | sort | tail -1)
printf '\x01\x02\x03\x04\x05\x06\x07\x08' | dd of="$NEWEST" bs=1 seek=65000 conv=notrunc 2>/dev/null
run "$OLD/tt-writer" "$J" 6100 65536 64
run "$OLD/tickertape" verify "$J"
run "$OLD/tickertape" inspect "$J"
run "$OLD/tickertape" archive "$J" --to "$W/dest" --keep-segments 2 --json
[ "$(ls "$W/dest" | wc -l)" -gt 0 ]
run "$OLD/tickertape" verify "$J"
run "$OLD/tickertape" export "$J" --format jsonl --output "$W/export.jsonl"
snapshot > "$W/after"
diff "$W/before" "$W/after"
echo "ok: v0.4.0 left readers/ unchanged"
