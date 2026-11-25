#!/bin/bash
# Checks that the released v1.0.0 tools are unaffected by seek indexes (ADR 0045, Compatibility with 1.0.0): builds the
# v1.0.0 tag and this checkout's `tickertape`, writes a journal with v1.0.0's writer, copies it, gives one copy indexes
# and other names in `seek-index/`, and runs the same v1.0.0 commands on both (verify, inspect, tail, stats, export,
# status, archive, a writer that continues and one that recovers a corrupt tail). Fails unless exit statuses, outputs
# and the records left are the same and every file in `seek-index/` is unchanged. Then exports with v1.0.0 while this
# checkout builds and removes indexes in a loop.
#
# Usage: tests/compat/v1.0.0-ignores-seek-index.sh [work_dir]   (from the repository root; needs git and cargo)
# Run weekly (.github/workflows/soak.yml).
# Builds into <work_dir>/target-v1.0.0 and <work_dir>/target; delete <work_dir> afterwards. EXPORTS sets how many exports
# run beside the loop (default 100).
set -euo pipefail
ROOT=$(pwd)
WORK=${1:-$(mktemp -d)}
mkdir -p "$WORK/v1.0.0"
git -C "$ROOT" archive v1.0.0 | tar -x -C "$WORK/v1.0.0"
(cd "$WORK/v1.0.0" && CARGO_TARGET_DIR="$WORK/target-v1.0.0" cargo build -q -p tickertape-cli -p tickertape-tests --bins)
CARGO_TARGET_DIR="$WORK/target" cargo build -q -p tickertape-cli --bins
OLD=$WORK/target-v1.0.0/debug
NEW=$WORK/target/debug/tickertape
W=$WORK/run
rm -rf "$W"
mkdir -p "$W/with/dest" "$W/without/dest"

"$OLD/tt-writer" "$W/without/journal" 9000 131072 64 > /dev/null
cp -Rp "$W/without/journal" "$W/with/journal"
J=$W/with/journal
# A directory under /tmp can have a group that is not this user's (macOS); `index` then refuses to make seek-index/.
chgrp "$(id -g)" "$J" "$J"/*
"$NEW" index "$J"
[ "$(find "$J/seek-index" -name '*.idx' | wc -l)" -gt 3 ]
# What else can have a name there: an unfinished index, a later kind, a user's file, a FIFO, a subdirectory.
echo x > "$J/seek-index/.00000000000000000000.1.2.idx-tmp"
echo x > "$J/seek-index/00000000000000000000.idx2"
echo notes > "$J/seek-index/notes"
mkfifo "$J/seek-index/fifo"
mkdir "$J/seek-index/sub"
snapshot() {
    (cd "$J/seek-index" && find . -print0 | sort -z | xargs -0 ls -ldT 2> /dev/null ||
        find . -print0 | sort -z | xargs -0 ls -ld --time-style=full-iso)
    (cd "$J/seek-index" && find . -type f -print0 | sort -z | xargs -0 shasum)
}
snapshot > "$W/before"

# Runs a v1.0.0 command in both, with the journal as ./journal, and compares status and output.
both() {
    echo "== $*"
    local side status
    for side in with without; do
        status=0
        (cd "$W/$side" && "$OLD/$1" "${@:2}" > "$W/$side.out" 2> "$W/$side.err") || status=$?
        echo "status $status" >> "$W/$side.out"
        [ -z "${EXPECT:-}" ] || [ "$status" = "$EXPECT" ]
    done
    # Masked: what depends on the clock (timestamps).
    local mask='s/[0-9]{10,}/T/g; s/"created_utc":"[^"]*"/"created_utc":T/g'
    diff <(sed -E "$mask" "$W/with.out") <(sed -E "$mask" "$W/without.out")
    diff "$W/with.err" "$W/without.err"
}
readers() {
    both tickertape verify journal --json
    both tickertape inspect journal --json
    both tickertape tail journal --no-follow --format json --from 4000
    both tickertape tail journal --no-follow --durable-only
    both tickertape stats journal --json --from 3000 --to 8999 # the records both copies share, clock and all
    both tickertape export journal --format jsonl --output export.jsonl
    diff <(sed -E 's/[0-9]{10,}/T/g' "$W/with/export.jsonl") <(sed -E 's/[0-9]{10,}/T/g' "$W/without/export.jsonl")
}
EXPECT=0 readers
EXPECT=0 both tickertape archive journal --to dest --keep-segments 5 --json
diff -r "$W/with/dest" "$W/without/dest"
EXPECT=0 both tt-writer journal 10000 131072 64
EXPECT=0 readers
# Damage after the checkpoint, for the next open's recovery.
for side in with without; do
    NEWEST=$(ls "$W/$side/journal"/*.tape | sort | tail -1)
    printf '\x01\x02\x03\x04\x05\x06\x07\x08' | dd of="$NEWEST" bs=1 seek=130000 conv=notrunc 2> /dev/null
done
EXPECT=0 both tt-writer journal 10100 131072 64
# No marker: recovery from the oldest segment. (An index-unaware writer leaves the indexes; they are checked by value.)
rm "$W/with/journal/checkpoint" "$W/without/journal/checkpoint"
EXPECT=0 both tt-writer journal 10200 131072 64
EXPECT=0 readers
snapshot > "$W/after"
diff "$W/before" "$W/after"
diff <(cd "$W/with/journal" && ls | grep -v seek-index) <(cd "$W/without/journal" && ls)
"$NEW" index "$J" --check > /dev/null

# Exports while indexes come and go. The directory itself stays: 1.0.0's walk fails if one vanishes (ADR 0045).
(while [ ! -e "$W/stop" ]; do "$NEW" index "$J" --rebuild > /dev/null; "$NEW" index "$J" --remove > /dev/null; done) &
LOOP=$!
trap 'touch "$W/stop"; wait $LOOP || true' EXIT
for _ in $(seq "${EXPORTS:-100}"); do
    "$OLD/tickertape" export "$J" --format jsonl --output "$W/loop.jsonl" --from 9000 > /dev/null
done
touch "$W/stop"
wait $LOOP
trap - EXIT
echo "ok: v1.0.0 is unaffected by seek-index/ and left it unchanged"
